use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Stdio,
    time::{Duration as StdDuration, Instant},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use futures_util::TryStreamExt;
use keyring::Entry;
use serde::{Deserialize, Serialize};
use tiberius::{AuthMethod, Client, Config};
use tokio::{io::AsyncWriteExt, net::TcpStream, process::Command, time::timeout};
use tokio_util::compat::TokioAsyncWriteCompatExt;

const SQL_KEYCHAIN_SERVICE: &str = "io.github.metasys-summary.dashboard.sql";
const SQL_KEYCHAIN_ACCOUNT: &str = "trend-source";
const MAX_TREND_ROWS: usize = 5_000;
const MAX_POINT_ROWS: usize = 10_000;
const MAX_SELECTED_POINTS: usize = 8;
pub const MAX_LIVE_POINT_VALUES: usize = 128;
pub const LIVE_VALUE_TARGET_REFRESH_MILLISECONDS: u64 = 1_000;
pub const LIVE_VALUE_MAX_REFRESH_MILLISECONDS: u64 = 60_000;
pub const LIVE_VALUE_LOOKBACK_HOURS: i64 = 24 * 30;
const LIVE_VALUE_POINTS_PER_SECOND: usize = 32;
const LIVE_VALUE_QUERY_HEADROOM: u64 = 4;
const MAX_TREND_HOURS: i64 = 24 * 365 * 10;
const LEGACY_HELPER_TIMEOUT_SECONDS: u64 = 40;
const LEGACY_ZONE_ONLY_FILTER: &str = "p.PointName LIKE '%.ZN-T.#85'";
const DEFAULT_HVAC_FILTER: &str = r#"(
        p.PointName LIKE '%.ZN-T.#85'
        OR p.PointName LIKE '%.SA-T.#85'
        OR p.PointName LIKE '%.SA-F.#85'
        OR p.PointName LIKE '%.SF-C.#85'
        OR p.PointName LIKE '%.SF-S.#85'
        OR p.PointName LIKE '%.DA-T.#85'
        OR p.PointName LIKE '%.HWV-O.#85'
        OR p.PointName LIKE '%.HTG-O.#85'
    )"#;
const DEFAULT_QUERY: &str = r#"SELECT
    CAST(recent.point_name AS nvarchar(512)) AS point_name,
    recent.sample_time,
    CAST(recent.sample_value AS float) AS sample_value,
    CAST(recent.unit AS nvarchar(64)) AS unit
FROM (
    SELECT TOP (5001)
        p.PointName AS point_name,
        valueset.UTCDateTime AS sample_time,
        valueset.sample_value,
        COALESCE(NULLIF(u.DisplayNameShort, ''), u.UnitOfMeasureName) AS unit
    FROM dbo.tblPoint AS p
    JOIN dbo.tblPointSlice AS ps ON ps.PointID = p.PointID
    LEFT JOIN dbo.tblUnitOfMeasure AS u ON u.UnitOfMeasureID = p.UnitOfMeasureID
    CROSS APPLY (
        SELECT UTCDateTime, CAST(ActualValue AS float) AS sample_value
        FROM dbo.tblActualValueFloat
        WHERE PointSliceID = ps.PointSliceID AND UTCDateTime >= @P1 AND UTCDateTime <= @P2
        UNION ALL
        SELECT UTCDateTime, CAST(ActualValue AS float) AS sample_value
        FROM dbo.tblActualValueDigital
        WHERE PointSliceID = ps.PointSliceID AND UTCDateTime >= @P1 AND UTCDateTime <= @P2
    ) AS valueset
    WHERE ps.IsRawData = 1 AND (
        p.PointName LIKE '%.ZN-T.#85'
        OR p.PointName LIKE '%.SA-T.#85'
        OR p.PointName LIKE '%.SA-F.#85'
        OR p.PointName LIKE '%.SF-C.#85'
        OR p.PointName LIKE '%.SF-S.#85'
        OR p.PointName LIKE '%.DA-T.#85'
        OR p.PointName LIKE '%.HWV-O.#85'
        OR p.PointName LIKE '%.HTG-O.#85'
    )
    ORDER BY valueset.UTCDateTime DESC
) AS recent
ORDER BY recent.sample_time ASC"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlTrendSettings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub trust_server_certificate: bool,
    #[serde(default)]
    pub legacy_tls: bool,
    pub query: String,
}

impl Default for SqlTrendSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 1433,
            database: String::new(),
            username: String::new(),
            trust_server_certificate: false,
            legacy_tls: false,
            query: DEFAULT_QUERY.to_owned(),
        }
    }
}

impl SqlTrendSettings {
    pub fn upgrade_legacy_defaults(mut self) -> Self {
        let legacy_default =
            DEFAULT_QUERY.replacen(DEFAULT_HVAC_FILTER, LEGACY_ZONE_ONLY_FILTER, 1);
        if self.query == legacy_default {
            self.query = DEFAULT_QUERY.to_owned();
        }
        self
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlTrendSettingsUpdate {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub trust_server_certificate: bool,
    #[serde(default)]
    pub legacy_tls: bool,
    pub query: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub clear_password: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlTrendSettingsView {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub trust_server_certificate: bool,
    pub legacy_tls: bool,
    pub query: String,
    pub password_configured: bool,
}

impl SqlTrendSettingsUpdate {
    pub fn validated_settings(&self) -> Result<SqlTrendSettings> {
        let settings = SqlTrendSettings {
            enabled: self.enabled,
            host: self.host.trim().to_owned(),
            port: self.port,
            database: self.database.trim().to_owned(),
            username: self.username.trim().to_owned(),
            trust_server_certificate: self.trust_server_certificate,
            legacy_tls: self.legacy_tls,
            query: self.query.trim().to_owned(),
        };
        settings.validate_for_storage()?;
        if self.clear_password
            && self
                .password
                .as_ref()
                .is_some_and(|value| !value.is_empty())
        {
            bail!("password and clearPassword cannot be supplied together");
        }
        Ok(settings)
    }
}

impl SqlTrendSettings {
    pub fn validate(&self) -> Result<()> {
        if self.port == 0 {
            bail!("SQL Server port must be between 1 and 65535");
        }
        validate_field("host", &self.host, 253)?;
        if self.host.contains("//") || self.host.contains('/') || self.host.contains('\\') {
            bail!("SQL Server host must be a hostname or IP address, without a URL scheme");
        }
        validate_field("database", &self.database, 128)?;
        validate_field("username", &self.username, 256)?;
        validate_read_only_query(&self.query)
    }

    fn validate_for_storage(&self) -> Result<()> {
        if self.enabled
            || !self.host.is_empty()
            || !self.database.is_empty()
            || !self.username.is_empty()
        {
            self.validate()
        } else {
            if self.port == 0 {
                bail!("SQL Server port must be between 1 and 65535");
            }
            validate_read_only_query(&self.query)
        }
    }

    pub fn view(&self) -> SqlTrendSettingsView {
        SqlTrendSettingsView {
            enabled: self.enabled,
            host: self.host.clone(),
            port: self.port,
            database: self.database.clone(),
            username: self.username.clone(),
            trust_server_certificate: self.trust_server_certificate,
            legacy_tls: self.legacy_tls,
            query: self.query.clone(),
            password_configured: sql_password_configured(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendPoint {
    pub point_slice_id: i32,
    pub point_name: String,
    pub unit: Option<String>,
    #[serde(default)]
    pub equipment_name: String,
    #[serde(default)]
    pub equipment_path: String,
    #[serde(default)]
    pub point_family: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendPointCatalog {
    pub points: Vec<TrendPoint>,
    pub truncated: bool,
}

pub const FEATURED_TREND_POINT_FAMILIES: &[&str] = &[
    "ZN-T", "SA-T", "SA-F", "SF-C", "SF-S", "DA-T", "HWV-O", "HTG-O", "CLG-O", "ZN-SP", "RA-T",
    "OA-T", "MA-T", "DMP-O",
];

fn build_point_catalog(mut points: Vec<TrendPoint>, truncated: bool) -> TrendPointCatalog {
    for point in &mut points {
        let (equipment_name, equipment_path, point_family) =
            trend_point_metadata(&point.point_name);
        point.equipment_name = equipment_name;
        point.equipment_path = equipment_path;
        point.point_family = point_family;
    }
    points.sort_by(|left, right| {
        left.equipment_path
            .cmp(&right.equipment_path)
            .then_with(|| left.point_family.cmp(&right.point_family))
            .then_with(|| left.point_name.cmp(&right.point_name))
    });
    TrendPointCatalog { points, truncated }
}

fn trend_point_metadata(point_name: &str) -> (String, String, String) {
    let mut reference = point_name.trim();
    if let Some((prefix, marker)) = reference.rsplit_once('.')
        && is_attribute_marker(marker)
    {
        reference = prefix.trim_end_matches('.');
    }

    let (equipment_path, family) = reference.rsplit_once('.').map_or_else(
        || {
            let uppercase = reference.to_ascii_uppercase();
            FEATURED_TREND_POINT_FAMILIES
                .iter()
                .find_map(|family| {
                    let marker = format!(".{family}");
                    uppercase
                        .rfind(&marker)
                        .map(|index| (&reference[..index], *family))
                })
                .unwrap_or((reference, ""))
        },
        |(equipment, family)| (equipment, family),
    );
    let equipment_path = equipment_path
        .trim()
        .trim_end_matches(['.', '/', ':'])
        .to_owned();
    let equipment_name = equipment_path
        .rsplit(['/', '.'])
        .find(|part| !part.trim().is_empty())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .unwrap_or("Unassigned equipment")
        .to_owned();
    let point_family = normalize_point_family(family);
    (equipment_name, equipment_path, point_family)
}

fn is_attribute_marker(value: &str) -> bool {
    value.strip_prefix('#').is_some_and(|digits| {
        !digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit())
    })
}

fn normalize_point_family(value: &str) -> String {
    let normalized = value.trim().replace(['_', ' '], "-").to_ascii_uppercase();
    if normalized.is_empty()
        || normalized.len() > 32
        || !normalized
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        String::new()
    } else {
        normalized
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendSample {
    pub timestamp: DateTime<Utc>,
    pub value: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendSeries {
    pub name: String,
    pub unit: Option<String>,
    pub samples: Vec<TrendSample>,
    pub statistics: TrendStatistics,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendStatistics {
    pub count: usize,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub average: Option<f64>,
    pub latest: Option<f64>,
    pub change: Option<f64>,
    pub rate_per_day: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendResponse {
    pub generated_at: DateTime<Utc>,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub sample_count: usize,
    pub truncated: bool,
    pub bucket_seconds: Option<i64>,
    pub aggregation: Option<&'static str>,
    pub series: Vec<TrendSeries>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LivePointValue {
    pub point_slice_id: i32,
    pub timestamp: DateTime<Utc>,
    pub value: f64,
    pub unit: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LivePointValuesResponse {
    pub generated_at: DateTime<Utc>,
    pub refresh_interval_milliseconds: u64,
    pub refresh_interval_seconds: u64,
    pub refresh_reason: &'static str,
    pub query_duration_milliseconds: u64,
    pub point_count: usize,
    pub source: &'static str,
    pub polls_mstp_trunk: bool,
    pub lookback_hours: i64,
    pub values: Vec<LivePointValue>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SampleRow {
    point_name: String,
    sample_time: DateTime<Utc>,
    sample_value: f64,
    unit: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacyConnection<'a> {
    host: &'a str,
    port: u16,
    database: &'a str,
    username: &'a str,
    trust_server_certificate: bool,
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "camelCase")]
enum LegacyRequest<'a> {
    Test {
        connection: LegacyConnection<'a>,
        password: String,
    },
    Query {
        connection: LegacyConnection<'a>,
        password: String,
        query: &'a str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    },
    Points {
        connection: LegacyConnection<'a>,
        password: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "result", rename_all = "camelCase")]
enum LegacyResponse {
    Connected,
    Samples {
        rows: Vec<SampleRow>,
        truncated: bool,
    },
    Points {
        points: Vec<TrendPoint>,
        truncated: bool,
    },
}

pub fn set_sql_password(password: &str) -> Result<()> {
    if password.is_empty() {
        bail!("SQL Server password cannot be empty");
    }
    sql_keychain_entry()?
        .set_password(password)
        .context("save SQL Server password in macOS Keychain")
}

pub fn clear_sql_password() -> Result<()> {
    if !sql_password_configured() {
        return Ok(());
    }
    sql_keychain_entry()?
        .delete_credential()
        .context("remove SQL Server password from macOS Keychain")
}

pub fn sql_password_configured() -> bool {
    read_sql_password().is_ok()
}

pub async fn test_connection(settings: &SqlTrendSettings) -> Result<()> {
    settings.validate()?;
    if settings.legacy_tls {
        let response = run_legacy_helper(LegacyRequest::Test {
            connection: legacy_connection(settings),
            password: read_sql_password()?,
        })
        .await?;
        if !matches!(response, LegacyResponse::Connected) {
            bail!("legacy SQL helper returned an unexpected connection-test response");
        }
        return Ok(());
    }

    let mut client = connect(settings).await?;
    client
        .simple_query("SELECT 1 AS connection_test")
        .await
        .context("run SQL Server connection test")?
        .into_row()
        .await
        .context("read SQL Server connection test")?
        .context("SQL Server connection test returned no data")?;
    Ok(())
}

pub async fn fetch_trend_points(settings: &SqlTrendSettings) -> Result<TrendPointCatalog> {
    settings.validate()?;
    if !settings.enabled {
        bail!("SQL trend source is disabled");
    }
    if settings.legacy_tls {
        return match run_legacy_helper(LegacyRequest::Points {
            connection: legacy_connection(settings),
            password: read_sql_password()?,
        })
        .await?
        {
            LegacyResponse::Points { points, truncated } => {
                Ok(build_point_catalog(points, truncated))
            }
            _ => bail!("legacy SQL helper returned an unexpected point-catalog response"),
        };
    }

    let mut client = connect(settings).await?;
    let query = client
        .simple_query(point_catalog_query())
        .await
        .context("query Metasys historian point catalog")?;
    let mut stream = query.into_row_stream();
    let mut points = Vec::new();
    let mut truncated = false;
    while let Some(row) = stream
        .try_next()
        .await
        .context("read historian point row")?
    {
        if points.len() >= MAX_POINT_ROWS {
            truncated = true;
            break;
        }
        points.push(TrendPoint {
            point_slice_id: row
                .try_get::<i32, _>("point_slice_id")?
                .context("point catalog returned a null point_slice_id")?,
            point_name: row
                .try_get::<&str, _>("point_name")?
                .context("point catalog returned a null point_name")?
                .to_owned(),
            unit: row
                .try_get::<&str, _>("unit")?
                .map(str::to_owned)
                .filter(|value| !value.trim().is_empty()),
            equipment_name: String::new(),
            equipment_path: String::new(),
            point_family: String::new(),
        });
    }
    Ok(build_point_catalog(points, truncated))
}

pub async fn fetch_trends(
    settings: &SqlTrendSettings,
    hours: i64,
    point_slice_ids: &[i32],
) -> Result<TrendResponse> {
    let hours = hours.clamp(1, MAX_TREND_HOURS);
    let to = Utc::now();
    let from = to - Duration::hours(hours);
    fetch_trends_window(settings, from, to, None, point_slice_ids).await
}

pub async fn fetch_trends_window(
    settings: &SqlTrendSettings,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    requested_bucket_seconds: Option<i64>,
    point_slice_ids: &[i32],
) -> Result<TrendResponse> {
    settings.validate()?;
    if !settings.enabled {
        bail!("SQL trend source is disabled");
    }
    let range_seconds = validate_trend_window(from, to)?;
    let point_slice_ids = validate_point_slice_ids(point_slice_ids)?;
    let selected_query = if point_slice_ids.is_empty() {
        None
    } else {
        Some(selected_point_query(
            &point_slice_ids,
            range_seconds,
            requested_bucket_seconds,
        )?)
    };
    let query = selected_query
        .as_ref()
        .map(|(query, _)| query.as_str())
        .unwrap_or(&settings.query);
    validate_read_only_query(query)?;

    let (rows, truncated) = fetch_sample_rows(settings, query, from, to).await?;

    let bucket_seconds = selected_query.as_ref().map(|(_, seconds)| *seconds);
    Ok(group_rows(
        rows,
        truncated,
        from,
        to,
        bucket_seconds,
        bucket_seconds.map(|_| "mean"),
    ))
}

pub async fn fetch_live_point_values(
    settings: &SqlTrendSettings,
    point_slice_ids: &[i32],
) -> Result<LivePointValuesResponse> {
    settings.validate()?;
    if !settings.enabled {
        bail!("SQL trend source is disabled");
    }
    let point_slice_ids = validate_live_point_slice_ids(point_slice_ids)?;
    if point_slice_ids.is_empty() {
        bail!("select at least one historian point");
    }
    let point_count = point_slice_ids.len();
    let query = latest_point_values_query(&point_slice_ids)?;
    validate_read_only_query(&query)?;
    let to = Utc::now();
    let from = to - Duration::hours(LIVE_VALUE_LOOKBACK_HOURS);
    let query_started_at = Instant::now();
    let (rows, truncated) = fetch_sample_rows(settings, &query, from, to).await?;
    let query_duration_milliseconds =
        u64::try_from(query_started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
    if truncated {
        bail!("latest-value query exceeded its bounded result size");
    }

    let requested = point_slice_ids.into_iter().collect::<BTreeSet<_>>();
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        let point_slice_id = row
            .point_name
            .parse::<i32>()
            .context("latest-value query returned an invalid point identifier")?;
        if !requested.contains(&point_slice_id) {
            bail!("latest-value query returned an unrequested point identifier");
        }
        if !row.sample_value.is_finite() {
            bail!("latest-value query returned a non-finite value");
        }
        values.push(LivePointValue {
            point_slice_id,
            timestamp: row.sample_time,
            value: row.sample_value,
            unit: row.unit,
        });
    }
    values.sort_by_key(|value| value.point_slice_id);
    let (refresh_interval_milliseconds, refresh_reason) =
        live_refresh_recommendation(point_count, query_duration_milliseconds);

    Ok(LivePointValuesResponse {
        generated_at: Utc::now(),
        refresh_interval_milliseconds,
        refresh_interval_seconds: refresh_interval_milliseconds.div_ceil(1_000),
        refresh_reason,
        query_duration_milliseconds,
        point_count,
        source: "metasysSqlHistorian",
        polls_mstp_trunk: false,
        lookback_hours: LIVE_VALUE_LOOKBACK_HOURS,
        values,
    })
}

fn live_refresh_recommendation(
    point_count: usize,
    query_duration_milliseconds: u64,
) -> (u64, &'static str) {
    let point_batches = point_count.max(1).div_ceil(LIVE_VALUE_POINTS_PER_SECOND);
    let point_interval = u64::try_from(point_batches)
        .unwrap_or(u64::MAX)
        .saturating_mul(LIVE_VALUE_TARGET_REFRESH_MILLISECONDS);
    let latency_interval = query_duration_milliseconds
        .saturating_mul(LIVE_VALUE_QUERY_HEADROOM)
        .max(LIVE_VALUE_TARGET_REFRESH_MILLISECONDS);
    let unconstrained = point_interval.max(latency_interval);
    let interval = unconstrained.clamp(
        LIVE_VALUE_TARGET_REFRESH_MILLISECONDS,
        LIVE_VALUE_MAX_REFRESH_MILLISECONDS,
    );
    let reason = if unconstrained > LIVE_VALUE_MAX_REFRESH_MILLISECONDS {
        "safetyCap"
    } else if latency_interval > point_interval {
        "queryLatency"
    } else if point_interval > LIVE_VALUE_TARGET_REFRESH_MILLISECONDS {
        "pointCount"
    } else {
        "oneSecondTarget"
    };
    (interval, reason)
}

async fn fetch_sample_rows(
    settings: &SqlTrendSettings,
    query: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<(Vec<SampleRow>, bool)> {
    if settings.legacy_tls {
        return match run_legacy_helper(LegacyRequest::Query {
            connection: legacy_connection(settings),
            password: read_sql_password()?,
            query,
            from,
            to,
        })
        .await?
        {
            LegacyResponse::Samples { rows, truncated } => Ok((rows, truncated)),
            _ => bail!("legacy SQL helper returned an unexpected trend-query response"),
        };
    }
    fetch_rows_modern(settings, query, from, to).await
}

async fn fetch_rows_modern(
    settings: &SqlTrendSettings,
    query: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<(Vec<SampleRow>, bool)> {
    let mut client = connect(settings).await?;
    let query = client
        .query(query, &[&from.naive_utc(), &to.naive_utc()])
        .await
        .context("run read-only Metasys trend query")?;
    let mut stream = query.into_row_stream();
    let mut rows = Vec::new();
    let mut truncated = false;
    while let Some(row) = stream
        .try_next()
        .await
        .context("read SQL Server trend row")?
    {
        if rows.len() >= MAX_TREND_ROWS {
            truncated = true;
            break;
        }
        rows.push(SampleRow {
            point_name: row
                .try_get::<&str, _>("point_name")
                .context("trend query column point_name must be text")?
                .context("trend query returned a null point_name")?
                .to_owned(),
            sample_time: read_timestamp(&row)?,
            sample_value: read_numeric_value(&row)?,
            unit: row
                .try_get::<&str, _>("unit")
                .context("trend query column unit must be text or null")?
                .map(str::to_owned)
                .filter(|value| !value.trim().is_empty()),
        });
    }
    Ok((rows, truncated))
}

fn group_rows(
    rows: Vec<SampleRow>,
    truncated: bool,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    bucket_seconds: Option<i64>,
    aggregation: Option<&'static str>,
) -> TrendResponse {
    let sample_count = rows.len();
    let mut grouped: BTreeMap<(String, Option<String>), Vec<TrendSample>> = BTreeMap::new();
    for row in rows {
        grouped
            .entry((row.point_name, row.unit))
            .or_default()
            .push(TrendSample {
                timestamp: row.sample_time,
                value: row.sample_value,
            });
    }
    let series = grouped
        .into_iter()
        .map(|((name, unit), mut samples)| {
            samples.sort_by_key(|sample| sample.timestamp);
            let statistics = trend_statistics(&samples);
            TrendSeries {
                name,
                unit,
                samples,
                statistics,
            }
        })
        .collect();
    TrendResponse {
        generated_at: Utc::now(),
        from,
        to,
        sample_count,
        truncated,
        bucket_seconds,
        aggregation,
        series,
    }
}

fn trend_statistics(samples: &[TrendSample]) -> TrendStatistics {
    if samples.is_empty() {
        return TrendStatistics {
            count: 0,
            minimum: None,
            maximum: None,
            average: None,
            latest: None,
            change: None,
            rate_per_day: None,
        };
    }
    let count = samples.len();
    let minimum = samples.iter().map(|sample| sample.value).reduce(f64::min);
    let maximum = samples.iter().map(|sample| sample.value).reduce(f64::max);
    let average = Some(samples.iter().map(|sample| sample.value).sum::<f64>() / count as f64);
    let latest = samples.last().map(|sample| sample.value);
    let change = samples
        .first()
        .zip(samples.last())
        .map(|(first, last)| last.value - first.value);
    let rate_per_day = linear_rate_per_day(samples);
    TrendStatistics {
        count,
        minimum,
        maximum,
        average,
        latest,
        change,
        rate_per_day,
    }
}

fn linear_rate_per_day(samples: &[TrendSample]) -> Option<f64> {
    if samples.len() < 2 {
        return None;
    }
    let origin = samples.first()?.timestamp;
    let count = samples.len() as f64;
    let (sum_x, sum_y, sum_xy, sum_x_squared) = samples.iter().fold(
        (0.0, 0.0, 0.0, 0.0),
        |(sum_x, sum_y, sum_xy, sum_x_squared), sample| {
            let x = (sample.timestamp - origin).num_milliseconds() as f64 / 1_000.0;
            (
                sum_x + x,
                sum_y + sample.value,
                sum_xy + x * sample.value,
                sum_x_squared + x * x,
            )
        },
    );
    let denominator = count * sum_x_squared - sum_x * sum_x;
    if denominator.abs() <= f64::EPSILON {
        return None;
    }
    Some(((count * sum_xy - sum_x * sum_y) / denominator) * 86_400.0)
}

async fn connect(
    settings: &SqlTrendSettings,
) -> Result<Client<tokio_util::compat::Compat<TcpStream>>> {
    let password = read_sql_password()?;
    let mut config = Config::new();
    config.host(&settings.host);
    config.port(settings.port);
    config.database(&settings.database);
    config.authentication(AuthMethod::sql_server(&settings.username, password));
    if settings.trust_server_certificate {
        config.trust_cert();
    }
    let address = config.get_addr();
    let tcp = timeout(StdDuration::from_secs(12), TcpStream::connect(&address))
        .await
        .with_context(|| format!("SQL Server connection to {address} timed out"))?
        .with_context(|| format!("connect to SQL Server at {address}"))?;
    tcp.set_nodelay(true)
        .context("configure SQL Server TCP connection")?;
    timeout(
        StdDuration::from_secs(15),
        Client::connect(config, tcp.compat_write()),
    )
    .await
    .context("SQL Server TLS/login timed out")?
    .context("authenticate to SQL Server")
}

async fn run_legacy_helper(request: LegacyRequest<'_>) -> Result<LegacyResponse> {
    let encoded = serde_json::to_vec(&request).context("encode legacy SQL request")?;
    let helper = legacy_helper_path()?;
    let mut child = Command::new(&helper)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start isolated legacy SQL helper {}", helper.display()))?;
    let mut stdin = child.stdin.take().context("open legacy SQL helper input")?;
    stdin
        .write_all(&encoded)
        .await
        .context("send legacy SQL request")?;
    stdin.shutdown().await.context("close legacy SQL request")?;
    drop(stdin);

    let output = timeout(
        StdDuration::from_secs(LEGACY_HELPER_TIMEOUT_SECONDS),
        child.wait_with_output(),
    )
    .await
    .context("legacy SQL helper timed out")?
    .context("wait for legacy SQL helper")?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr)
            .trim()
            .chars()
            .take(2_000)
            .collect::<String>();
        bail!(
            "legacy SQL connection failed{}",
            if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            }
        );
    }
    serde_json::from_slice(&output.stdout).context("decode legacy SQL helper response")
}

fn legacy_helper_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("METASYS_LEGACY_SQL_HELPER") {
        return Ok(PathBuf::from(path));
    }
    let executable = std::env::current_exe().context("locate dashboard executable")?;
    let adjacent = executable
        .parent()
        .context("dashboard executable has no parent directory")?
        .join("metasys-sql-legacy-helper");
    if adjacent.is_file() {
        return Ok(adjacent);
    }
    let development = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("legacy-sql-helper/target/debug/metasys-sql-legacy-helper");
    if development.is_file() {
        return Ok(development);
    }
    bail!(
        "legacy TLS helper is not installed next to the dashboard executable ({})",
        adjacent.display()
    )
}

fn legacy_connection(settings: &SqlTrendSettings) -> LegacyConnection<'_> {
    LegacyConnection {
        host: &settings.host,
        port: settings.port,
        database: &settings.database,
        username: &settings.username,
        trust_server_certificate: settings.trust_server_certificate,
    }
}

fn read_sql_password() -> Result<String> {
    if let Ok(password) = std::env::var("METASYS_SQL_PASSWORD")
        && !password.is_empty()
    {
        return Ok(password);
    }
    sql_keychain_entry()?
        .get_password()
        .context("SQL Server password is missing; save it from SQL Trend Settings")
}

fn sql_keychain_entry() -> Result<Entry> {
    Entry::new(SQL_KEYCHAIN_SERVICE, SQL_KEYCHAIN_ACCOUNT)
        .context("open SQL Server password entry in macOS Keychain")
}

fn point_catalog_query() -> &'static str {
    r#"SELECT TOP (10001)
    ps.PointSliceID AS point_slice_id,
    CAST(p.PointName AS nvarchar(400)) AS point_name,
    CAST(COALESCE(NULLIF(u.DisplayNameShort, ''), u.UnitOfMeasureName) AS nvarchar(64)) AS unit
FROM dbo.tblPointSlice AS ps
JOIN dbo.tblPoint AS p ON p.PointID = ps.PointID
LEFT JOIN dbo.tblUnitOfMeasure AS u ON u.UnitOfMeasureID = p.UnitOfMeasureID
WHERE ps.IsRawData = 1
ORDER BY p.PointName"#
}

fn selected_point_query(
    point_slice_ids: &[i32],
    range_seconds: i64,
    requested_bucket_seconds: Option<i64>,
) -> Result<(String, i64)> {
    let point_slice_ids = validate_point_slice_ids(point_slice_ids)?;
    let point_count = point_slice_ids.len();
    let ids = point_slice_ids
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if ids.is_empty() {
        bail!("select at least one historian point");
    }
    let buckets_per_series = (MAX_TREND_ROWS / point_count).saturating_sub(1).max(1);
    let minimum_bucket_seconds =
        ((range_seconds + buckets_per_series as i64 - 1) / buckets_per_series as i64).max(1);
    let requested_bucket_seconds = requested_bucket_seconds.unwrap_or(minimum_bucket_seconds);
    if !(1..=MAX_TREND_HOURS * 60 * 60).contains(&requested_bucket_seconds) {
        bail!("trend interval must be between 1 second and 10 years");
    }
    let bucket_seconds = requested_bucket_seconds.max(minimum_bucket_seconds);
    Ok((
        format!(
            r#"WITH source_values AS (
        SELECT PointSliceID, UTCDateTime, CAST(ActualValue AS float) AS sample_value
        FROM dbo.tblActualValueFloat
        WHERE PointSliceID IN ({ids}) AND UTCDateTime >= @P1 AND UTCDateTime <= @P2
        UNION ALL
        SELECT PointSliceID, UTCDateTime, CAST(ActualValue AS float) AS sample_value
        FROM dbo.tblActualValueDigital
        WHERE PointSliceID IN ({ids}) AND UTCDateTime >= @P1 AND UTCDateTime <= @P2
), bucketed_values AS (
    SELECT
        PointSliceID,
        DATEADD(
            SECOND,
            (DATEDIFF(SECOND, @P1, UTCDateTime) / {bucket_seconds}) * {bucket_seconds},
            @P1
        ) AS sample_time,
        AVG(sample_value) AS sample_value
    FROM source_values
    GROUP BY
        PointSliceID,
        DATEDIFF(SECOND, @P1, UTCDateTime) / {bucket_seconds}
)
SELECT
    CAST(p.PointName AS nvarchar(512)) AS point_name,
    bucketed_values.sample_time,
    CAST(bucketed_values.sample_value AS float) AS sample_value,
    CAST(COALESCE(NULLIF(u.DisplayNameShort, ''), u.UnitOfMeasureName) AS nvarchar(64)) AS unit
FROM bucketed_values
JOIN dbo.tblPointSlice AS ps ON ps.PointSliceID = bucketed_values.PointSliceID
JOIN dbo.tblPoint AS p ON p.PointID = ps.PointID
LEFT JOIN dbo.tblUnitOfMeasure AS u ON u.UnitOfMeasureID = p.UnitOfMeasureID
ORDER BY bucketed_values.sample_time ASC"#
        ),
        bucket_seconds,
    ))
}

fn latest_point_values_query(point_slice_ids: &[i32]) -> Result<String> {
    let point_slice_ids = validate_live_point_slice_ids(point_slice_ids)?;
    let ids = point_slice_ids
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(",");
    if ids.is_empty() {
        bail!("select at least one historian point");
    }
    Ok(format!(
        r#"SELECT
    CAST(ps.PointSliceID AS nvarchar(32)) AS point_name,
    latest.sample_time,
    CAST(latest.sample_value AS float) AS sample_value,
    CAST(COALESCE(NULLIF(u.DisplayNameShort, ''), u.UnitOfMeasureName) AS nvarchar(64)) AS unit
FROM dbo.tblPointSlice AS ps
JOIN dbo.tblPoint AS p ON p.PointID = ps.PointID
LEFT JOIN dbo.tblUnitOfMeasure AS u ON u.UnitOfMeasureID = p.UnitOfMeasureID
CROSS APPLY (
    SELECT TOP (1)
        source_values.sample_time,
        source_values.sample_value
    FROM (
        SELECT UTCDateTime AS sample_time, CAST(ActualValue AS float) AS sample_value
        FROM dbo.tblActualValueFloat
        WHERE PointSliceID = ps.PointSliceID AND UTCDateTime >= @P1 AND UTCDateTime <= @P2
        UNION ALL
        SELECT UTCDateTime AS sample_time, CAST(ActualValue AS float) AS sample_value
        FROM dbo.tblActualValueDigital
        WHERE PointSliceID = ps.PointSliceID AND UTCDateTime >= @P1 AND UTCDateTime <= @P2
    ) AS source_values
    ORDER BY source_values.sample_time DESC
) AS latest
WHERE ps.PointSliceID IN ({ids})
ORDER BY ps.PointSliceID"#
    ))
}

fn validate_trend_window(from: DateTime<Utc>, to: DateTime<Utc>) -> Result<i64> {
    let range_seconds = (to - from).num_seconds();
    if range_seconds < 1 {
        bail!("trend start must be before trend end");
    }
    if range_seconds > MAX_TREND_HOURS * 60 * 60 {
        bail!("trend range cannot exceed 10 years");
    }
    Ok(range_seconds)
}

fn validate_point_slice_ids(point_slice_ids: &[i32]) -> Result<Vec<i32>> {
    validate_point_slice_ids_with_limit(point_slice_ids, MAX_SELECTED_POINTS)
}

fn validate_live_point_slice_ids(point_slice_ids: &[i32]) -> Result<Vec<i32>> {
    validate_point_slice_ids_with_limit(point_slice_ids, MAX_LIVE_POINT_VALUES)
}

fn validate_point_slice_ids_with_limit(
    point_slice_ids: &[i32],
    maximum: usize,
) -> Result<Vec<i32>> {
    let unique = point_slice_ids
        .iter()
        .copied()
        .filter(|value| *value > 0)
        .collect::<BTreeSet<_>>();
    if unique.len() > maximum {
        bail!("select no more than {maximum} historian points");
    }
    if unique.len() != point_slice_ids.len() {
        bail!("historian point selections must be unique positive identifiers");
    }
    Ok(unique.into_iter().collect())
}

fn read_timestamp(row: &tiberius::Row) -> Result<DateTime<Utc>> {
    if let Ok(Some(value)) = row.try_get::<NaiveDateTime, _>("sample_time") {
        return Ok(DateTime::from_naive_utc_and_offset(value, Utc));
    }
    row.try_get::<DateTime<Utc>, _>("sample_time")
        .context("trend query column sample_time must be datetime/datetime2/datetimeoffset")?
        .context("trend query returned a null sample_time")
}

fn read_numeric_value(row: &tiberius::Row) -> Result<f64> {
    if let Ok(Some(value)) = row.try_get::<f64, _>("sample_value") {
        return Ok(value);
    }
    if let Ok(Some(value)) = row.try_get::<f32, _>("sample_value") {
        return Ok(f64::from(value));
    }
    if let Ok(Some(value)) = row.try_get::<i64, _>("sample_value") {
        return Ok(value as f64);
    }
    if let Ok(Some(value)) = row.try_get::<i32, _>("sample_value") {
        return Ok(f64::from(value));
    }
    if let Ok(Some(value)) = row.try_get::<i16, _>("sample_value") {
        return Ok(f64::from(value));
    }
    bail!("trend query column sample_value must be a non-null numeric value")
}

fn validate_field(name: &str, value: &str, maximum_length: usize) -> Result<()> {
    if value.is_empty() {
        bail!("SQL Server {name} is required");
    }
    if value.len() > maximum_length || value.chars().any(char::is_control) {
        bail!("SQL Server {name} is invalid");
    }
    Ok(())
}

fn validate_read_only_query(query: &str) -> Result<()> {
    let trimmed = query.trim();
    if trimmed.is_empty() || trimmed.len() > 8_192 {
        bail!("trend query must contain 1 to 8192 characters");
    }
    let lowercase = trimmed.to_ascii_lowercase();
    if !matches!(lowercase.split_whitespace().next(), Some("select" | "with")) {
        bail!("trend query must start with SELECT or WITH");
    }
    if trimmed.contains(';') || lowercase.contains("--") || lowercase.contains("/*") {
        bail!("trend query must contain one statement without comments");
    }
    for token in lowercase
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
    {
        if matches!(
            token,
            "insert"
                | "update"
                | "delete"
                | "merge"
                | "execute"
                | "exec"
                | "drop"
                | "alter"
                | "create"
                | "truncate"
                | "grant"
                | "revoke"
                | "deny"
                | "into"
                | "openrowset"
                | "opendatasource"
                | "openquery"
                | "bulk"
                | "xp_cmdshell"
                | "sp_oacreate"
                | "shutdown"
                | "backup"
                | "restore"
                | "waitfor"
                | "kill"
                | "dbcc"
        ) {
            bail!("trend query contains a forbidden write or external-access keyword");
        }
    }
    if !lowercase.contains("@p1") || !lowercase.contains("@p2") {
        bail!("trend query must use @P1 (start) and @P2 (end) parameters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_HVAC_FILTER, LEGACY_ZONE_ONLY_FILTER, SampleRow, SqlTrendSettings, group_rows,
        latest_point_values_query, live_refresh_recommendation, selected_point_query,
        trend_point_metadata, validate_live_point_slice_ids, validate_point_slice_ids,
        validate_read_only_query, validate_trend_window,
    };
    use chrono::{Duration, TimeZone, Utc};

    #[test]
    fn default_settings_query_is_valid() {
        let settings = SqlTrendSettings::default();
        settings.validate_for_storage().unwrap();
        settings.validate().unwrap_err();
        validate_read_only_query(&settings.query).unwrap();
    }

    #[test]
    fn blocks_write_multi_statement_and_wait_queries() {
        assert!(
            validate_read_only_query("DELETE FROM samples WHERE t >= @P1 AND t <= @P2").is_err()
        );
        assert!(
            validate_read_only_query(
                "SELECT * FROM samples WHERE t >= @P1 AND t <= @P2; DROP TABLE samples"
            )
            .is_err()
        );
        assert!(
            validate_read_only_query(
                "SELECT * INTO copied FROM samples WHERE t >= @P1 AND t <= @P2"
            )
            .is_err()
        );
        assert!(
            validate_read_only_query(
                "SELECT * FROM samples WHERE t >= @P1 AND t <= @P2 WAITFOR DELAY '00:01'"
            )
            .is_err()
        );
    }

    #[test]
    fn selected_point_query_is_bounded_and_parameterized() {
        let (query, bucket_seconds) =
            selected_point_query(&[42, 7], 24 * 365 * 60 * 60, Some(60)).unwrap();
        validate_read_only_query(&query).unwrap();
        assert!(query.contains("PointSliceID IN (7,42)"));
        assert!(query.contains("AVG(sample_value)"));
        assert!(query.contains("DATEDIFF(SECOND, @P1, UTCDateTime)"));
        assert!(bucket_seconds > 60);
        assert!(validate_point_slice_ids(&[1, 1]).is_err());
        assert!(validate_point_slice_ids(&[0]).is_err());
        assert!(validate_point_slice_ids(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).is_err());
    }

    #[test]
    fn latest_value_query_is_read_only_bounded_and_uses_identifiers() {
        let query = latest_point_values_query(&[42, 7]).unwrap();
        validate_read_only_query(&query).unwrap();
        assert!(query.contains("ps.PointSliceID IN (7,42)"));
        assert!(query.contains("SELECT TOP (1)"));
        assert!(query.contains("ORDER BY source_values.sample_time DESC"));
        assert!(query.contains("UTCDateTime >= @P1 AND UTCDateTime <= @P2"));
        assert!(validate_live_point_slice_ids(&[1, 1]).is_err());
        assert!(validate_live_point_slice_ids(&[0]).is_err());
        assert!(validate_live_point_slice_ids(&(1..=129).collect::<Vec<_>>()).is_err());
    }

    #[test]
    fn live_refresh_targets_one_second_and_scales_with_point_count() {
        assert_eq!(
            live_refresh_recommendation(1, 20),
            (1_000, "oneSecondTarget")
        );
        assert_eq!(
            live_refresh_recommendation(32, 20),
            (1_000, "oneSecondTarget")
        );
        assert_eq!(live_refresh_recommendation(33, 20), (2_000, "pointCount"));
        assert_eq!(live_refresh_recommendation(128, 20), (4_000, "pointCount"));
    }

    #[test]
    fn live_refresh_reserves_query_headroom_and_caps_backoff() {
        assert_eq!(live_refresh_recommendation(4, 750), (3_000, "queryLatency"));
        assert_eq!(
            live_refresh_recommendation(4, 20_000),
            (60_000, "safetyCap")
        );
    }

    #[test]
    fn classifies_terminal_box_and_air_handler_point_names() {
        assert_eq!(
            trend_point_metadata("G2-NAE:G2-NAE/FC-1.TB6-P06.ZN-T.#85"),
            (
                "TB6-P06".to_owned(),
                "G2-NAE:G2-NAE/FC-1.TB6-P06".to_owned(),
                "ZN-T".to_owned(),
            )
        );
        assert_eq!(
            trend_point_metadata("BMSServer:CentralPlant/FC-1.AHU-1.SA-F.#85"),
            (
                "AHU-1".to_owned(),
                "BMSServer:CentralPlant/FC-1.AHU-1".to_owned(),
                "SA-F".to_owned(),
            )
        );
        assert_eq!(
            trend_point_metadata("SERVER:DEVICE/FAV-201.HWV_O"),
            (
                "FAV-201".to_owned(),
                "SERVER:DEVICE/FAV-201".to_owned(),
                "HWV-O".to_owned(),
            )
        );

        for family in [
            "ZN-T", "SA-T", "SA-F", "SF-C", "SF-S", "DA-T", "HWV-O", "HTG-O",
        ] {
            let reference = format!("BMSServer:A2-NAE/FC-1.TB1-301.{family}.#85");
            let (equipment, path, discovered_family) = trend_point_metadata(&reference);
            assert_eq!(equipment, "TB1-301");
            assert_eq!(path, "BMSServer:A2-NAE/FC-1.TB1-301");
            assert_eq!(discovered_family, family);
        }
    }

    #[test]
    fn trend_window_rejects_reversed_and_excessive_ranges() {
        let now = Utc::now();
        assert!(validate_trend_window(now, now).is_err());
        assert!(validate_trend_window(now, now - Duration::seconds(1)).is_err());
        assert!(validate_trend_window(now - Duration::days(365 * 11), now).is_err());
        assert_eq!(
            validate_trend_window(now - Duration::hours(24), now).unwrap(),
            86_400
        );
    }

    #[test]
    fn response_includes_statistics_and_linear_rate() {
        let from = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let to = from + Duration::days(2);
        let rows = vec![
            SampleRow {
                point_name: "Zone temperature".to_owned(),
                sample_time: from,
                sample_value: 68.0,
                unit: Some("deg F".to_owned()),
            },
            SampleRow {
                point_name: "Zone temperature".to_owned(),
                sample_time: from + Duration::days(1),
                sample_value: 70.0,
                unit: Some("deg F".to_owned()),
            },
            SampleRow {
                point_name: "Zone temperature".to_owned(),
                sample_time: to,
                sample_value: 72.0,
                unit: Some("deg F".to_owned()),
            },
        ];
        let response = group_rows(rows, false, from, to, Some(300), Some("mean"));
        let statistics = &response.series[0].statistics;
        assert_eq!(statistics.count, 3);
        assert_eq!(statistics.minimum, Some(68.0));
        assert_eq!(statistics.maximum, Some(72.0));
        assert_eq!(statistics.average, Some(70.0));
        assert_eq!(statistics.latest, Some(72.0));
        assert_eq!(statistics.change, Some(4.0));
        assert!((statistics.rate_per_day.unwrap() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn old_settings_without_legacy_flag_remain_compatible() {
        let value = serde_json::json!({
            "enabled": true,
            "host": "sql.example.invalid",
            "port": 1433,
            "database": "JCIHistorianDB",
            "username": "reader",
            "trustServerCertificate": true,
            "query": SqlTrendSettings::default().query
        });
        let settings: SqlTrendSettings = serde_json::from_value(value).unwrap();
        assert!(!settings.legacy_tls);
    }

    #[test]
    fn upgrades_the_zone_only_built_in_query_to_featured_hvac_families() {
        let settings = SqlTrendSettings {
            query: super::DEFAULT_QUERY.replacen(DEFAULT_HVAC_FILTER, LEGACY_ZONE_ONLY_FILTER, 1),
            ..Default::default()
        }
        .upgrade_legacy_defaults();
        assert!(settings.query.contains(DEFAULT_HVAC_FILTER));
        for family in [
            "ZN-T", "SA-T", "SA-F", "SF-C", "SF-S", "DA-T", "HWV-O", "HTG-O",
        ] {
            assert!(settings.query.contains(family));
        }
        validate_read_only_query(&settings.query).unwrap();

        let custom_query = format!(
            "SELECT * FROM samples WHERE sample_time >= @P1 AND sample_time <= @P2 AND {LEGACY_ZONE_ONLY_FILTER}"
        );
        let custom_settings = SqlTrendSettings {
            query: custom_query.clone(),
            ..Default::default()
        }
        .upgrade_legacy_defaults();
        assert_eq!(custom_settings.query, custom_query);
    }
}
