use std::{collections::BTreeMap, time::Duration as StdDuration};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use futures_util::TryStreamExt;
use keyring::Entry;
use serde::{Deserialize, Serialize};
use tiberius::{AuthMethod, Client, Config};
use tokio::{net::TcpStream, time::timeout};
use tokio_util::compat::TokioAsyncWriteCompatExt;

const SQL_KEYCHAIN_SERVICE: &str = "io.github.metasys-summary.dashboard.sql";
const SQL_KEYCHAIN_ACCOUNT: &str = "trend-source";
const MAX_TREND_ROWS: usize = 5_000;
const DEFAULT_QUERY: &str = r#"SELECT TOP (5000)
    CAST(point_name AS nvarchar(512)) AS point_name,
    sample_time,
    CAST(sample_value AS float) AS sample_value,
    CAST(unit AS nvarchar(64)) AS unit
FROM dbo.MetasysTrendSamples
WHERE sample_time >= @P1 AND sample_time <= @P2
ORDER BY sample_time ASC"#;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlTrendSettings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    pub trust_server_certificate: bool,
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
            query: self.query.clone(),
            password_configured: sql_password_configured(),
        }
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

pub async fn fetch_trends(settings: &SqlTrendSettings, hours: i64) -> Result<TrendResponse> {
    settings.validate()?;
    if !settings.enabled {
        bail!("SQL trend source is disabled");
    }
    let to = Utc::now();
    let from = to - Duration::hours(hours.clamp(1, 24 * 31));
    let mut client = connect(settings).await?;
    let query = client
        .query(&settings.query, &[&from.naive_utc(), &to.naive_utc()])
        .await
        .context("run read-only Metasys trend query")?;
    let mut rows = query.into_row_stream();
    let mut grouped: BTreeMap<(String, Option<String>), Vec<TrendSample>> = BTreeMap::new();
    let mut sample_count = 0;
    let mut truncated = false;

    while let Some(row) = rows.try_next().await.context("read SQL Server trend row")? {
        if sample_count >= MAX_TREND_ROWS {
            truncated = true;
            break;
        }
        let point_name = row
            .try_get::<&str, _>("point_name")
            .context("trend query column point_name must be text")?
            .context("trend query returned a null point_name")?
            .to_owned();
        let timestamp = read_timestamp(&row)?;
        let value = read_numeric_value(&row)?;
        let unit = row
            .try_get::<&str, _>("unit")
            .context("trend query column unit must be text or null")?
            .map(str::to_owned)
            .filter(|value| !value.trim().is_empty());
        grouped
            .entry((point_name, unit))
            .or_default()
            .push(TrendSample { timestamp, value });
        sample_count += 1;
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

    Ok(TrendResponse {
        generated_at: Utc::now(),
        from,
        to,
        sample_count,
        truncated,
        series,
    })
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

fn read_sql_password() -> Result<String> {
    sql_keychain_entry()?
        .get_password()
        .context("SQL Server password is missing; save it from SQL Trend Settings")
}

fn sql_keychain_entry() -> Result<Entry> {
    Entry::new(SQL_KEYCHAIN_SERVICE, SQL_KEYCHAIN_ACCOUNT)
        .context("open SQL Server password entry in macOS Keychain")
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
    if !(lowercase.starts_with("select ") || lowercase.starts_with("with ")) {
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
    use super::{SqlTrendSettings, validate_read_only_query};

    #[test]
    fn default_settings_query_is_valid() {
        let settings = SqlTrendSettings::default();
        settings.validate_for_storage().unwrap();
        settings.validate().unwrap_err();
        validate_read_only_query(&settings.query).unwrap();
    }

    #[test]
    fn blocks_write_and_multi_statement_queries() {
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
    }
}
