use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Stdio,
    time::Duration as StdDuration,
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
const MAX_TREND_HOURS: i64 = 24 * 365 * 10;
const LEGACY_HELPER_TIMEOUT_SECONDS: u64 = 40;
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
    WHERE ps.IsRawData = 1 AND p.PointName LIKE '%.ZN-T.#85'
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
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendPointCatalog {
    pub points: Vec<TrendPoint>,
    pub truncated: bool,
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
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrendResponse {
    pub generated_at: DateTime<Utc>,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub sample_count: usize,
    pub truncated: bool,
    pub series: Vec<TrendSeries>,
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
                Ok(TrendPointCatalog { points, truncated })
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
        });
    }
    Ok(TrendPointCatalog { points, truncated })
}

pub async fn fetch_trends(
    settings: &SqlTrendSettings,
    hours: i64,
    point_slice_ids: &[i32],
) -> Result<TrendResponse> {
    settings.validate()?;
    if !settings.enabled {
        bail!("SQL trend source is disabled");
    }
    let hours = hours.clamp(1, MAX_TREND_HOURS);
    let to = Utc::now();
    let from = to - Duration::hours(hours);
    let point_slice_ids = validate_point_slice_ids(point_slice_ids)?;
    let selected_query = (!point_slice_ids.is_empty())
        .then(|| selected_point_query(&point_slice_ids, hours))
        .transpose()?;
    let query = selected_query.as_deref().unwrap_or(&settings.query);
    validate_read_only_query(query)?;

    let (rows, truncated) = if settings.legacy_tls {
        match run_legacy_helper(LegacyRequest::Query {
            connection: legacy_connection(settings),
            password: read_sql_password()?,
            query,
            from,
            to,
        })
        .await?
        {
            LegacyResponse::Samples { rows, truncated } => (rows, truncated),
            _ => bail!("legacy SQL helper returned an unexpected trend-query response"),
        }
    } else {
        fetch_rows_modern(settings, query, from, to).await?
    };

    Ok(group_rows(rows, truncated, from, to))
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
            TrendSeries {
                name,
                unit,
                samples,
            }
        })
        .collect();
    TrendResponse {
        generated_at: Utc::now(),
        from,
        to,
        sample_count,
        truncated,
        series,
    }
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

fn selected_point_query(point_slice_ids: &[i32], hours: i64) -> Result<String> {
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
    let range_seconds = hours.clamp(1, MAX_TREND_HOURS) * 60 * 60;
    let bucket_seconds =
        ((range_seconds + buckets_per_series as i64 - 1) / buckets_per_series as i64).max(1);
    Ok(format!(
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
    ))
}

fn validate_point_slice_ids(point_slice_ids: &[i32]) -> Result<Vec<i32>> {
    let unique = point_slice_ids
        .iter()
        .copied()
        .filter(|value| *value > 0)
        .collect::<BTreeSet<_>>();
    if unique.len() > MAX_SELECTED_POINTS {
        bail!("select no more than {MAX_SELECTED_POINTS} historian points");
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
        SqlTrendSettings, selected_point_query, validate_point_slice_ids, validate_read_only_query,
    };

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
        let query = selected_point_query(&[42, 7], 24 * 365).unwrap();
        validate_read_only_query(&query).unwrap();
        assert!(query.contains("PointSliceID IN (7,42)"));
        assert!(query.contains("AVG(sample_value)"));
        assert!(query.contains("DATEDIFF(SECOND, @P1, UTCDateTime)"));
        assert!(validate_point_slice_ids(&[1, 1]).is_err());
        assert!(validate_point_slice_ids(&[0]).is_err());
        assert!(validate_point_slice_ids(&[1, 2, 3, 4, 5, 6, 7, 8, 9]).is_err());
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
}
