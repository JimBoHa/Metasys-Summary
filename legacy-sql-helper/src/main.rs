use std::{env, io::Write, time::Duration as StdDuration};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tiberius::{AuthMethod, Client, Config};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_util::compat::TokioAsyncWriteCompatExt;

const MAX_REQUEST_BYTES: u64 = 32 * 1024;
const MAX_TREND_ROWS: usize = 5_000;
const MAX_POINT_ROWS: usize = 10_000;
const LEGACY_OPENSSL_CONFIG: &str = r#"openssl_conf = default_conf

[default_conf]
ssl_conf = ssl_sect

[ssl_sect]
system_default = system_default_sect

[system_default_sect]
MinProtocol = TLSv1
CipherString = DEFAULT:@SECLEVEL=0
"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectionSpec {
    host: String,
    port: u16,
    database: String,
    username: String,
    trust_server_certificate: bool,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "camelCase")]
enum Request {
    Test {
        connection: ConnectionSpec,
        password: String,
    },
    Query {
        connection: ConnectionSpec,
        password: String,
        query: String,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    },
    Points {
        connection: ConnectionSpec,
        password: String,
    },
}

#[derive(Serialize)]
#[serde(tag = "result", rename_all = "camelCase")]
enum Response {
    Connected,
    Samples {
        rows: Vec<SampleRow>,
        truncated: bool,
    },
    Points {
        points: Vec<PointRow>,
        truncated: bool,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SampleRow {
    point_name: String,
    sample_time: DateTime<Utc>,
    sample_value: f64,
    unit: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PointRow {
    point_slice_id: i32,
    point_name: String,
    unit: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _legacy_config = configure_legacy_tls()?;
    let mut body = String::new();
    tokio::io::stdin()
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_string(&mut body)
        .await
        .context("read legacy SQL request")?;
    if body.len() as u64 > MAX_REQUEST_BYTES {
        bail!("legacy SQL request is too large");
    }

    let request: Request = serde_json::from_str(&body).context("decode legacy SQL request")?;
    let response = match request {
        Request::Test {
            connection,
            password,
        } => {
            let mut client = connect(&connection, password).await?;
            client
                .simple_query("SELECT 1 AS connection_test")
                .await
                .context("run SQL Server connection test")?
                .into_row()
                .await
                .context("read SQL Server connection test")?
                .context("SQL Server connection test returned no data")?;
            Response::Connected
        }
        Request::Query {
            connection,
            password,
            query,
            from,
            to,
        } => {
            validate_read_only_query(&query)?;
            let mut client = connect(&connection, password).await?;
            let query = client
                .query(&query, &[&from.naive_utc(), &to.naive_utc()])
                .await
                .context("run read-only Metasys trend query")?;
            let mut stream = query.into_row_stream();
            let mut rows = Vec::new();
            let mut truncated = false;
            while let Some(row) = stream.try_next().await.context("read SQL trend row")? {
                if rows.len() >= MAX_TREND_ROWS {
                    truncated = true;
                    break;
                }
                let point_name = row
                    .try_get::<&str, _>("point_name")
                    .context("trend query column point_name must be text")?
                    .context("trend query returned a null point_name")?
                    .to_owned();
                let sample_time = read_timestamp(&row)?;
                let sample_value = read_numeric_value(&row)?;
                let unit = row
                    .try_get::<&str, _>("unit")
                    .context("trend query column unit must be text or null")?
                    .map(str::to_owned)
                    .filter(|value| !value.trim().is_empty());
                rows.push(SampleRow {
                    point_name,
                    sample_time,
                    sample_value,
                    unit,
                });
            }
            Response::Samples { rows, truncated }
        }
        Request::Points {
            connection,
            password,
        } => {
            let mut client = connect(&connection, password).await?;
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
                points.push(PointRow {
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
            Response::Points { points, truncated }
        }
    };

    let encoded = serde_json::to_vec(&response).context("encode legacy SQL response")?;
    tokio::io::stdout()
        .write_all(&encoded)
        .await
        .context("write legacy SQL response")?;
    Ok(())
}

fn configure_legacy_tls() -> Result<NamedTempFile> {
    let mut file = NamedTempFile::new().context("create isolated legacy TLS configuration")?;
    file.write_all(LEGACY_OPENSSL_CONFIG.as_bytes())
        .context("write isolated legacy TLS configuration")?;
    env::set_var("OPENSSL_CONF", file.path());
    Ok(file)
}

async fn connect(
    connection: &ConnectionSpec,
    password: String,
) -> Result<Client<tokio_util::compat::Compat<TcpStream>>> {
    validate_connection(connection)?;
    let mut config = Config::new();
    config.host(&connection.host);
    config.port(connection.port);
    config.database(&connection.database);
    config.authentication(AuthMethod::sql_server(&connection.username, password));
    if connection.trust_server_certificate {
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
        StdDuration::from_secs(20),
        Client::connect(config, tcp.compat_write()),
    )
    .await
    .context("legacy SQL Server TLS/login timed out")?
    .context("authenticate to SQL Server")
}

fn validate_connection(connection: &ConnectionSpec) -> Result<()> {
    if connection.port == 0 {
        bail!("SQL Server port must be between 1 and 65535");
    }
    for (name, value, maximum) in [
        ("host", connection.host.as_str(), 253),
        ("database", connection.database.as_str(), 128),
        ("username", connection.username.as_str(), 256),
    ] {
        if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
            bail!("SQL Server {name} is invalid");
        }
    }
    if connection.host.contains("//")
        || connection.host.contains('/')
        || connection.host.contains('\\')
    {
        bail!("SQL Server host must be a hostname or IP address");
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

#[cfg(test)]
mod tests {
    use super::validate_read_only_query;

    #[test]
    fn accepts_bounded_select_and_cte_queries() {
        validate_read_only_query(
            "SELECT * FROM samples WHERE sample_time >= @P1 AND sample_time <= @P2",
        )
        .unwrap();
        validate_read_only_query(
            "WITH samples AS (SELECT * FROM source WHERE t >= @P1 AND t <= @P2) SELECT * FROM samples",
        )
        .unwrap();
    }

    #[test]
    fn rejects_writes_comments_extra_statements_and_delays() {
        for query in [
            "DELETE FROM samples WHERE t >= @P1 AND t <= @P2",
            "SELECT * FROM samples WHERE t >= @P1 AND t <= @P2; DROP TABLE samples",
            "SELECT * FROM samples -- @P1 @P2",
            "SELECT * FROM samples WHERE t >= @P1 AND t <= @P2 WAITFOR DELAY '00:01'",
        ] {
            assert!(validate_read_only_query(query).is_err(), "accepted {query}");
        }
    }
}
