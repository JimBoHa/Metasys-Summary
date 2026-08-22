mod mirror;

use std::{
    env, fmt,
    fs::File,
    io::{Read, Write},
    mem::ManuallyDrop,
    os::fd::FromRawFd,
    path::PathBuf,
    time::Duration as StdDuration,
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tiberius::{AuthMethod, Client, Config};
use tokio::{net::TcpStream, time::timeout};
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

#[derive(Clone, Deserialize)]
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
    Inspect {
        connection: ConnectionSpec,
        password: String,
    },
    Mirror {
        connection: ConnectionSpec,
        password: String,
        target_database: PathBuf,
        volume_marker: PathBuf,
        batch_size: usize,
        max_event_rows: Option<u64>,
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
    Inspection {
        inspection: HistorianDatabaseInspection,
    },
    Mirror {
        report: mirror::MirrorReport,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistorianTableInfo {
    table_schema: String,
    table_name: String,
    row_count: i64,
    reserved_bytes: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistorianColumnInfo {
    table_schema: String,
    table_name: String,
    ordinal_position: i32,
    column_name: String,
    data_type: String,
    max_length: i16,
    precision: u8,
    scale: u8,
    nullable: bool,
    identity: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistorianIndexColumnInfo {
    table_schema: String,
    table_name: String,
    index_name: String,
    primary_key: bool,
    unique: bool,
    key_ordinal: u8,
    column_name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistorianDatabaseInspection {
    tables: Vec<HistorianTableInfo>,
    columns: Vec<HistorianColumnInfo>,
    index_columns: Vec<HistorianIndexColumnInfo>,
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
async fn main() {
    if let Err(error) = run().await {
        write_stderr(format_args!("Error: {error:#}"));
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let _legacy_config = configure_legacy_tls()?;
    let mut body = String::new();
    // Use the inherited descriptor directly. On macOS, constructing Stdin can
    // reopen /dev/stdin; reopening a pipe after the parent has already closed
    // its writer can block forever under launchd.
    let stdin = unsafe { File::from_raw_fd(0) };
    stdin
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_string(&mut body)
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
        Request::Inspect {
            connection,
            password,
        } => Response::Inspection {
            inspection: inspect_historian_database(&connection, password).await?,
        },
        Request::Mirror {
            connection,
            password,
            target_database,
            volume_marker,
            batch_size,
            max_event_rows,
        } => {
            write_stderr(format_args!("Legacy SQL mirror request accepted"));
            Response::Mirror {
                report: mirror::mirror_historian_database(
                    &connection,
                    password,
                    &target_database,
                    &volume_marker,
                    batch_size,
                    max_event_rows,
                )
                .await?,
            }
        }
    };

    let encoded = serde_json::to_vec(&response).context("encode legacy SQL response")?;
    let mut stdout = unsafe { File::from_raw_fd(1) };
    stdout
        .write_all(&encoded)
        .context("write legacy SQL response")?;
    Ok(())
}

fn write_stderr(message: fmt::Arguments<'_>) {
    // SAFETY: launchd supplies descriptor 2; this temporary File is prevented
    // from closing the process-wide descriptor when it leaves scope.
    let mut stderr = ManuallyDrop::new(unsafe { File::from_raw_fd(2) });
    let _ = writeln!(&mut *stderr, "{message}");
}

async fn inspect_historian_database(
    connection: &ConnectionSpec,
    password: String,
) -> Result<HistorianDatabaseInspection> {
    let mut client = connect(connection, password).await?;
    let query = client
        .simple_query(
            r#"
            SELECT
                CAST(s.name AS nvarchar(128)) AS table_schema,
                CAST(t.name AS nvarchar(128)) AS table_name,
                counts.row_count,
                storage.reserved_bytes
            FROM sys.tables AS t
            JOIN sys.schemas AS s ON s.schema_id = t.schema_id
            CROSS APPLY (
                SELECT CAST(COALESCE(SUM(CASE WHEN p.index_id IN (0, 1) THEN p.rows ELSE 0 END), 0) AS bigint) AS row_count
                FROM sys.partitions AS p
                WHERE p.object_id = t.object_id
            ) AS counts
            CROSS APPLY (
                SELECT CAST(COALESCE(SUM(a.total_pages), 0) * 8192 AS bigint) AS reserved_bytes
                FROM sys.partitions AS p
                JOIN sys.allocation_units AS a
                  ON a.container_id = p.hobt_id OR a.container_id = p.partition_id
                WHERE p.object_id = t.object_id
            ) AS storage
            WHERE t.is_ms_shipped = 0
            ORDER BY s.name, t.name
            "#,
        )
        .await
        .context("inspect SQL Server historian tables")?;
    let mut rows = query.into_row_stream();
    let mut tables = Vec::new();
    while let Some(row) = rows
        .try_next()
        .await
        .context("read historian table metadata")?
    {
        tables.push(HistorianTableInfo {
            table_schema: required_text(&row, "table_schema")?,
            table_name: required_text(&row, "table_name")?,
            row_count: row
                .try_get::<i64, _>("row_count")?
                .context("historian table metadata returned a null row_count")?,
            reserved_bytes: row
                .try_get::<i64, _>("reserved_bytes")?
                .context("historian table metadata returned a null reserved_bytes")?,
        });
    }
    drop(rows);

    let query = client
        .simple_query(
            r#"
            SELECT
                CAST(s.name AS nvarchar(128)) AS table_schema,
                CAST(t.name AS nvarchar(128)) AS table_name,
                c.column_id AS ordinal_position,
                CAST(c.name AS nvarchar(128)) AS column_name,
                CAST(ty.name AS nvarchar(128)) AS data_type,
                c.max_length,
                c.precision,
                c.scale,
                c.is_nullable,
                c.is_identity
            FROM sys.tables AS t
            JOIN sys.schemas AS s ON s.schema_id = t.schema_id
            JOIN sys.columns AS c ON c.object_id = t.object_id
            JOIN sys.types AS ty ON ty.user_type_id = c.user_type_id
            WHERE t.is_ms_shipped = 0
            ORDER BY s.name, t.name, c.column_id
            "#,
        )
        .await
        .context("inspect SQL Server historian columns")?;
    let mut rows = query.into_row_stream();
    let mut columns = Vec::new();
    while let Some(row) = rows
        .try_next()
        .await
        .context("read historian column metadata")?
    {
        columns.push(HistorianColumnInfo {
            table_schema: required_text(&row, "table_schema")?,
            table_name: required_text(&row, "table_name")?,
            ordinal_position: row
                .try_get::<i32, _>("ordinal_position")?
                .context("historian column metadata returned a null ordinal_position")?,
            column_name: required_text(&row, "column_name")?,
            data_type: required_text(&row, "data_type")?,
            max_length: row
                .try_get::<i16, _>("max_length")?
                .context("historian column metadata returned a null max_length")?,
            precision: row
                .try_get::<u8, _>("precision")?
                .context("historian column metadata returned a null precision")?,
            scale: row
                .try_get::<u8, _>("scale")?
                .context("historian column metadata returned a null scale")?,
            nullable: row
                .try_get::<bool, _>("is_nullable")?
                .context("historian column metadata returned a null is_nullable")?,
            identity: row
                .try_get::<bool, _>("is_identity")?
                .context("historian column metadata returned a null is_identity")?,
        });
    }
    drop(rows);

    let query = client
        .simple_query(
            r#"
            SELECT
                CAST(s.name AS nvarchar(128)) AS table_schema,
                CAST(t.name AS nvarchar(128)) AS table_name,
                CAST(i.name AS nvarchar(128)) AS index_name,
                i.is_primary_key,
                i.is_unique,
                ic.key_ordinal,
                CAST(c.name AS nvarchar(128)) AS column_name
            FROM sys.tables AS t
            JOIN sys.schemas AS s ON s.schema_id = t.schema_id
            JOIN sys.indexes AS i ON i.object_id = t.object_id
            JOIN sys.index_columns AS ic
              ON ic.object_id = i.object_id AND ic.index_id = i.index_id
            JOIN sys.columns AS c
              ON c.object_id = ic.object_id AND c.column_id = ic.column_id
            WHERE t.is_ms_shipped = 0
              AND i.is_hypothetical = 0
              AND ic.is_included_column = 0
              AND ic.key_ordinal > 0
            ORDER BY s.name, t.name, i.index_id, ic.key_ordinal
            "#,
        )
        .await
        .context("inspect SQL Server historian indexes")?;
    let mut rows = query.into_row_stream();
    let mut index_columns = Vec::new();
    while let Some(row) = rows
        .try_next()
        .await
        .context("read historian index metadata")?
    {
        index_columns.push(HistorianIndexColumnInfo {
            table_schema: required_text(&row, "table_schema")?,
            table_name: required_text(&row, "table_name")?,
            index_name: required_text(&row, "index_name")?,
            primary_key: row
                .try_get::<bool, _>("is_primary_key")?
                .context("historian index metadata returned a null is_primary_key")?,
            unique: row
                .try_get::<bool, _>("is_unique")?
                .context("historian index metadata returned a null is_unique")?,
            key_ordinal: row
                .try_get::<u8, _>("key_ordinal")?
                .context("historian index metadata returned a null key_ordinal")?,
            column_name: required_text(&row, "column_name")?,
        });
    }

    Ok(HistorianDatabaseInspection {
        tables,
        columns,
        index_columns,
    })
}

fn required_text(row: &tiberius::Row, column: &str) -> Result<String> {
    row.try_get::<&str, _>(column)?
        .map(str::to_owned)
        .with_context(|| format!("historian metadata returned a null {column}"))
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
