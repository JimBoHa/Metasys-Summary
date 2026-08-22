use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc};
use duckdb::{
    appender_params_from_iter, params,
    types::{Decimal, TimeUnit, Value},
    Connection, OptionalExt,
};
use futures_util::TryStreamExt;
use serde::Serialize;
use tiberius::Row;

use super::{
    connect, inspect_historian_database, write_stderr, ConnectionSpec, HistorianColumnInfo,
    HistorianDatabaseInspection, HistorianTableInfo,
};

const VOLUME_MARKER_CONTENT: &str = "METASYS_SUMMARY_EXTERNAL_STORAGE_V1\n";
const TARGET_SCHEMA: &str = "jci_historian";
const CONTROL_SCHEMA: &str = "metasys_migration";
const MIN_BATCH_SIZE: usize = 1_000;
const MAX_BATCH_SIZE: usize = 1_000_000;
const EVENT_TABLES: [&str; 4] = [
    "tblActualValueFloat",
    "tblActualValueDigital",
    "tblOtherValueFloat",
    "tblOtherValueDigital",
];
const REPORTING_DATABASE: &str = "JCIReportingDB";
const REPORTING_SCHEMA: &str = "jci_reporting";
const REPORTING_DATA_TABLE: &str = "tblDataItem";
const AUXILIARY_DATABASES: [(&str, &str); 4] = [
    ("JCIAuditTrails", "jci_audit_trails"),
    ("JCIEvents", "jci_events"),
    ("JCIItemAnnotation", "jci_item_annotation"),
    ("MetasysReporting", "metasys_reporting"),
];
const MIRRORED_DATABASE_SCHEMA_PREFIX: &str = "sql_server__";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MirrorReport {
    target_database: String,
    source_database: String,
    catalog_tables_copied: usize,
    catalog_rows_copied: u64,
    event_rows_copied_this_run: u64,
    event_rows_copied_total: u64,
    source_event_rows_at_start: u64,
    full_pass_completed: bool,
    stopped_by_row_limit: bool,
    operational_databases_mirrored: usize,
    operational_rows_copied_this_run: u64,
    reporting_rows_copied_total: u64,
    operational_pass_completed: bool,
    duration_seconds: f64,
}

#[derive(Default)]
struct OperationalMirrorSummary {
    database_count: usize,
    rows_copied: u64,
    reporting_rows_total: u64,
    completed: bool,
}

#[derive(Clone, Copy, Debug)]
enum EventKind {
    ActualFloat,
    ActualDigital,
    OtherFloat,
    OtherDigital,
}

impl EventKind {
    fn all() -> [Self; 4] {
        [
            Self::ActualFloat,
            Self::ActualDigital,
            Self::OtherFloat,
            Self::OtherDigital,
        ]
    }

    fn table_name(self) -> &'static str {
        match self {
            Self::ActualFloat => "tblActualValueFloat",
            Self::ActualDigital => "tblActualValueDigital",
            Self::OtherFloat => "tblOtherValueFloat",
            Self::OtherDigital => "tblOtherValueDigital",
        }
    }

    fn select_columns(self) -> &'static str {
        match self {
            Self::ActualFloat | Self::ActualDigital => {
                "[PointSliceID], [UTCDateTime], [ActualValue]"
            }
            Self::OtherFloat | Self::OtherDigital => {
                "[PointSliceID], [UTCDateTime], [OtherValue], [ValueCategoryID], [Status]"
            }
        }
    }
}

#[derive(Debug)]
struct EventRow {
    point_slice_id: i32,
    utc_micros: i64,
    value: EventValue,
    value_category_id: Option<i32>,
    status: Option<i32>,
}

struct ReportingRow {
    values: Vec<Value>,
    utc_micros: i64,
    data_item_id: i32,
}

#[derive(Debug)]
enum EventValue {
    Float(f32),
    Digital(i16),
}

pub(super) async fn mirror_historian_database(
    source: &ConnectionSpec,
    password: String,
    target_path: &Path,
    volume_marker: &Path,
    batch_size: usize,
    max_event_rows: Option<u64>,
) -> Result<MirrorReport> {
    if !(MIN_BATCH_SIZE..=MAX_BATCH_SIZE).contains(&batch_size) {
        bail!("mirror batch size must be between {MIN_BATCH_SIZE} and {MAX_BATCH_SIZE} rows");
    }
    validate_external_target(target_path, volume_marker)?;
    let parent = target_path
        .parent()
        .context("DuckDB mirror target has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create DuckDB mirror directory {}", parent.display()))?;
    validate_external_target(target_path, volume_marker)?;

    let started = Instant::now();
    write_stderr(format_args!(
        "Starting SQL historian mirror: {} -> {}",
        source.database,
        target_path.display()
    ));
    let inspection = inspect_historian_database(source, password.clone()).await?;
    let source_event_rows = inspection
        .tables
        .iter()
        .filter(|table| EVENT_TABLES.contains(&table.table_name.as_str()))
        .map(|table| u64::try_from(table.row_count).unwrap_or(0))
        .sum();

    let mut target = Connection::open(target_path)
        .with_context(|| format!("open DuckDB mirror {}", target_path.display()))?;
    initialize_target(&target)?;
    verify_source_identity(&target, source)?;
    record_schema_snapshot(&target, &inspection)?;
    let run_id = begin_run(&target, source, max_event_rows)?;

    let result = mirror_run(
        source,
        password,
        &inspection,
        &mut target,
        batch_size,
        max_event_rows,
        source_event_rows,
        target_path,
        started,
    )
    .await;

    match result {
        Ok(report) => {
            finish_run(&target, &run_id, "completed", None, &report)?;
            target
                .execute_batch("CHECKPOINT")
                .context("checkpoint DuckDB mirror")?;
            Ok(report)
        }
        Err(error) => {
            let message = error.to_string();
            let _ = finish_run_error(&target, &run_id, &message);
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn mirror_run(
    source: &ConnectionSpec,
    password: String,
    inspection: &HistorianDatabaseInspection,
    target: &mut Connection,
    batch_size: usize,
    max_event_rows: Option<u64>,
    source_event_rows: u64,
    target_path: &Path,
    started: Instant,
) -> Result<MirrorReport> {
    let mut client = connect(source, password.clone()).await?;
    let mut catalog_tables = 0usize;
    let mut catalog_rows = 0u64;

    for table in inspection
        .tables
        .iter()
        .filter(|table| !EVENT_TABLES.contains(&table.table_name.as_str()))
    {
        let columns = table_columns(inspection, table);
        let copied =
            copy_catalog_table(&mut client, target, TARGET_SCHEMA, table, &columns).await?;
        catalog_tables += 1;
        catalog_rows = catalog_rows.saturating_add(copied);
        write_stderr(format_args!(
            "Catalog mirrored: {}.{} ({} rows)",
            table.table_schema, table.table_name, copied
        ));
    }

    ensure_event_tables(target, inspection)?;
    let point_ids = read_point_slice_ids(target)?;
    write_stderr(format_args!(
        "Beginning resumable event copy for {} point slices across {} value tables",
        point_ids.len(),
        EVENT_TABLES.len()
    ));

    let mut copied_this_run = 0u64;
    let mut stopped_by_limit = false;
    let mut last_progress = Instant::now();
    for point_slice_id in point_ids {
        for kind in EventKind::all() {
            let remaining = max_event_rows.map(|limit| limit.saturating_sub(copied_this_run));
            if remaining == Some(0) {
                stopped_by_limit = true;
                break;
            }
            let copied = copy_event_point(
                &mut client,
                target,
                kind,
                point_slice_id,
                batch_size,
                remaining,
            )
            .await?;
            copied_this_run = copied_this_run.saturating_add(copied);

            if last_progress.elapsed() >= Duration::from_secs(15) || copied >= 1_000_000 {
                let total = total_event_rows(target)?;
                let elapsed = started.elapsed().as_secs_f64().max(0.001);
                write_stderr(format_args!(
                    "Event mirror progress: {} rows this run, {} total, {:.0} rows/sec",
                    copied_this_run,
                    total,
                    copied_this_run as f64 / elapsed
                ));
                last_progress = Instant::now();
            }
        }
        if stopped_by_limit {
            break;
        }
    }

    let full_pass_completed = !stopped_by_limit;
    let total = total_event_rows(target)?;
    if full_pass_completed {
        if total < source_event_rows {
            bail!(
                "full mirror pass copied {total} event rows, but the source contained at least {source_event_rows} when the pass started"
            );
        }
        target.execute(
            "INSERT INTO metasys_migration.full_passes (completed_at, source_event_rows_at_start, target_event_rows) VALUES (current_timestamp, ?, ?)",
            params![source_event_rows as i64, total as i64],
        )?;
    }
    drop(client);
    let operational =
        mirror_operational_databases(source, &password, target, batch_size, max_event_rows).await?;
    let report = MirrorReport {
        target_database: target_path.display().to_string(),
        source_database: source.database.clone(),
        catalog_tables_copied: catalog_tables,
        catalog_rows_copied: catalog_rows,
        event_rows_copied_this_run: copied_this_run,
        event_rows_copied_total: total,
        source_event_rows_at_start: source_event_rows,
        full_pass_completed,
        stopped_by_row_limit: stopped_by_limit,
        operational_databases_mirrored: operational.database_count,
        operational_rows_copied_this_run: operational.rows_copied,
        reporting_rows_copied_total: operational.reporting_rows_total,
        operational_pass_completed: operational.completed,
        duration_seconds: started.elapsed().as_secs_f64(),
    };
    write_stderr(format_args!(
        "SQL historian mirror cycle {}: {} event rows copied this run, {} total",
        if full_pass_completed {
            "completed"
        } else {
            "paused at requested validation limit"
        },
        copied_this_run,
        total
    ));
    Ok(report)
}

async fn mirror_operational_databases(
    base_source: &ConnectionSpec,
    password: &str,
    target: &mut Connection,
    batch_size: usize,
    max_reporting_rows: Option<u64>,
) -> Result<OperationalMirrorSummary> {
    write_stderr(format_args!(
        "Beginning operational Metasys SQL database mirror"
    ));
    let mut summary = OperationalMirrorSummary::default();
    for (database, target_schema) in AUXILIARY_DATABASES {
        let rows = mirror_snapshot_database(base_source, password, target, database, target_schema)
            .await?;
        summary.database_count += 1;
        summary.rows_copied = summary.rows_copied.saturating_add(rows);
        write_stderr(format_args!(
            "Operational database mirrored: {database} ({rows} rows)"
        ));
    }

    for database in list_user_databases(base_source, password).await? {
        if database.eq_ignore_ascii_case(&base_source.database)
            || database.eq_ignore_ascii_case(REPORTING_DATABASE)
            || AUXILIARY_DATABASES
                .iter()
                .any(|(known, _)| database.eq_ignore_ascii_case(known))
        {
            continue;
        }
        let target_schema = format!("{MIRRORED_DATABASE_SCHEMA_PREFIX}{database}");
        let rows =
            mirror_snapshot_database(base_source, password, target, &database, &target_schema)
                .await?;
        summary.database_count += 1;
        summary.rows_copied = summary.rows_copied.saturating_add(rows);
        write_stderr(format_args!(
            "Additional SQL database mirrored: {database} ({rows} rows)"
        ));
    }

    let (
        reporting_rows_copied,
        reporting_rows_total,
        reporting_source_rows,
        reporting_pass_completed,
    ) = mirror_reporting_database(
        base_source,
        password,
        target,
        batch_size,
        max_reporting_rows,
    )
    .await?;
    summary.database_count += 1;
    summary.rows_copied = summary.rows_copied.saturating_add(reporting_rows_copied);
    summary.reporting_rows_total = reporting_rows_total;
    summary.completed = reporting_pass_completed;
    if reporting_pass_completed {
        target.execute(
            &format!(
                "INSERT INTO {CONTROL_SCHEMA}.operational_passes (completed_at, database_count, rows_copied_this_run, reporting_source_rows_at_start, reporting_target_rows) VALUES (current_timestamp, ?, ?, ?, ?)"
            ),
            params![
                summary.database_count as i32,
                summary.rows_copied as i64,
                reporting_source_rows as i64,
                reporting_rows_total as i64
            ],
        )?;
    }
    write_stderr(format_args!(
        "Operational Metasys SQL mirror {}: {} databases, {} rows copied this run",
        if reporting_pass_completed {
            "completed"
        } else {
            "paused at requested validation limit"
        },
        summary.database_count,
        summary.rows_copied
    ));
    Ok(summary)
}

async fn list_user_databases(base_source: &ConnectionSpec, password: &str) -> Result<Vec<String>> {
    let mut source = base_source.clone();
    source.database = "master".to_owned();
    let mut client = connect(&source, password.to_owned()).await?;
    let query = client
        .simple_query(
            r#"
            SELECT CAST([name] AS nvarchar(128)) AS database_name
              FROM sys.databases
             WHERE database_id > 4
               AND [state] = 0
               AND HAS_DBACCESS([name]) = 1
             ORDER BY [name]
            "#,
        )
        .await
        .context("list accessible SQL Server user databases")?;
    let mut rows = query.into_row_stream();
    let mut databases = Vec::new();
    while let Some(row) = rows
        .try_next()
        .await
        .context("read SQL Server user database list")?
    {
        databases.push(
            row.try_get::<&str, _>("database_name")?
                .context("SQL Server database list returned a null name")?
                .to_owned(),
        );
    }
    Ok(databases)
}

fn register_database_mapping(
    target: &Connection,
    source_database: &str,
    target_schema: &str,
) -> Result<()> {
    target.execute(
        &format!(
            "INSERT INTO {CONTROL_SCHEMA}.source_database_mappings (source_database, target_schema, updated_at) VALUES (?, ?, now()) ON CONFLICT (source_database) DO UPDATE SET target_schema = excluded.target_schema, updated_at = now()"
        ),
        params![source_database, target_schema],
    )?;
    Ok(())
}

async fn mirror_snapshot_database(
    base_source: &ConnectionSpec,
    password: &str,
    target: &mut Connection,
    database: &str,
    target_schema: &str,
) -> Result<u64> {
    let mut source = base_source.clone();
    source.database = database.to_owned();
    let inspection = inspect_historian_database(&source, password.to_owned()).await?;
    record_database_schema_snapshot(target, database, &inspection)?;
    register_database_mapping(target, database, target_schema)?;
    target.execute_batch(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_duck_identifier(target_schema)
    ))?;
    let mut client = connect(&source, password.to_owned()).await?;
    let mut copied = 0u64;
    for table in &inspection.tables {
        let columns = table_columns(&inspection, table);
        copied = copied.saturating_add(
            copy_catalog_table(&mut client, target, target_schema, table, &columns).await?,
        );
    }
    Ok(copied)
}

async fn mirror_reporting_database(
    base_source: &ConnectionSpec,
    password: &str,
    target: &mut Connection,
    batch_size: usize,
    max_rows: Option<u64>,
) -> Result<(u64, u64, u64, bool)> {
    let mut source = base_source.clone();
    source.database = REPORTING_DATABASE.to_owned();
    let inspection = inspect_historian_database(&source, password.to_owned()).await?;
    record_database_schema_snapshot(target, REPORTING_DATABASE, &inspection)?;
    register_database_mapping(target, REPORTING_DATABASE, REPORTING_SCHEMA)?;
    target.execute_batch(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_duck_identifier(REPORTING_SCHEMA)
    ))?;
    let data_table = inspection
        .tables
        .iter()
        .find(|table| table.table_name == REPORTING_DATA_TABLE)
        .context("JCI reporting data table is missing")?;
    let source_data_rows = u64::try_from(data_table.row_count)
        .context("JCI reporting source row count is negative")?;
    let data_columns = table_columns(&inspection, data_table);
    let mut client = connect(&source, password.to_owned()).await?;
    let mut copied = 0u64;
    for table in inspection
        .tables
        .iter()
        .filter(|table| table.table_name != REPORTING_DATA_TABLE)
    {
        let columns = table_columns(&inspection, table);
        copied = copied.saturating_add(
            copy_catalog_table(&mut client, target, REPORTING_SCHEMA, table, &columns).await?,
        );
    }
    let ddl = create_table_sql(REPORTING_SCHEMA, REPORTING_DATA_TABLE, &data_columns)?;
    target.execute_batch(&ddl.replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS"))?;
    let point_ids = read_reporting_point_ids(target)?;
    let started = Instant::now();
    let mut last_progress = Instant::now();
    let mut data_copied = 0u64;
    write_stderr(format_args!(
        "Beginning resumable JCI reporting copy for {} points",
        point_ids.len()
    ));
    for point_id in point_ids {
        let remaining = max_rows.map(|limit| limit.saturating_sub(data_copied));
        if remaining == Some(0) {
            break;
        }
        let rows = copy_reporting_point(
            &mut client,
            target,
            point_id,
            &data_columns,
            batch_size,
            remaining,
        )
        .await?;
        data_copied = data_copied.saturating_add(rows);
        if last_progress.elapsed() >= Duration::from_secs(15) || rows >= 1_000_000 {
            write_stderr(format_args!(
                "Reporting mirror progress: {} rows this run, {} total, {:.0} rows/sec",
                data_copied,
                total_reporting_rows(target)?,
                data_copied as f64 / started.elapsed().as_secs_f64().max(0.001)
            ));
            last_progress = Instant::now();
        }
    }
    let total = total_reporting_rows(target)?;
    let stopped_by_limit = max_rows.is_some_and(|limit| data_copied >= limit);
    if !stopped_by_limit && total < source_data_rows {
        bail!(
            "full reporting pass copied {total} data rows, but the source contained at least {source_data_rows} when the pass started"
        );
    }
    copied = copied.saturating_add(data_copied);
    write_stderr(format_args!(
        "JCI reporting database mirrored: {data_copied} data rows this run, {total} total"
    ));
    Ok((copied, total, source_data_rows, !stopped_by_limit))
}

fn record_database_schema_snapshot(
    target: &Connection,
    database: &str,
    inspection: &HistorianDatabaseInspection,
) -> Result<()> {
    let encoded = serde_json::to_string(inspection).context("encode database schema snapshot")?;
    target.execute(
        &format!(
            "INSERT INTO {CONTROL_SCHEMA}.database_schema_snapshots (source_database, captured_at, schema_json) SELECT ?, current_timestamp, ? WHERE NOT EXISTS (SELECT 1 FROM {CONTROL_SCHEMA}.database_schema_snapshots WHERE source_database = ? AND schema_json = ?)"
        ),
        params![database, encoded, database, encoded],
    )?;
    Ok(())
}

fn read_reporting_point_ids(target: &Connection) -> Result<Vec<i32>> {
    let mut statement = target.prepare(&format!(
        "SELECT PointID FROM {REPORTING_SCHEMA}.tblPoint ORDER BY PointID"
    ))?;
    let rows = statement.query_map([], |row| row.get::<_, i32>(0))?;
    rows.collect::<duckdb::Result<Vec<_>>>()
        .context("read JCI reporting point catalog")
}

async fn copy_reporting_point(
    source: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
    target: &mut Connection,
    point_id: i32,
    columns: &[&HistorianColumnInfo],
    batch_size: usize,
    max_rows: Option<u64>,
) -> Result<u64> {
    let cursor = reporting_checkpoint(target, point_id)?;
    let select_columns = columns
        .iter()
        .map(|column| quote_sql_server_identifier(&column.column_name))
        .collect::<Vec<_>>()
        .join(", ");
    let query = if cursor.is_some() {
        format!(
            "SELECT {select_columns} FROM [dbo].[{REPORTING_DATA_TABLE}] WHERE [PointID] = @P1 AND ([DataTimeStamp] > @P2 OR ([DataTimeStamp] = @P2 AND [DataItemID] > @P3)) ORDER BY [DataTimeStamp], [DataItemID]"
        )
    } else {
        format!(
            "SELECT {select_columns} FROM [dbo].[{REPORTING_DATA_TABLE}] WHERE [PointID] = @P1 ORDER BY [DataTimeStamp], [DataItemID]"
        )
    };
    let query = if let Some((utc_micros, data_item_id)) = cursor {
        let timestamp = DateTime::<Utc>::from_timestamp_micros(utc_micros)
            .context("reporting checkpoint is outside the supported timestamp range")?
            .naive_utc();
        source
            .query(query, &[&point_id, &timestamp, &data_item_id])
            .await?
    } else {
        source.query(query, &[&point_id]).await?
    };
    let mut stream = query.into_row_stream();
    let mut batch = Vec::with_capacity(batch_size);
    let mut copied = 0u64;
    while let Some(row) = stream
        .try_next()
        .await
        .with_context(|| format!("stream JCI reporting point {point_id}"))?
    {
        let data_item_id = row
            .try_get::<i32, _>(0)?
            .context("reporting row has a null DataItemID")?;
        let timestamp = row
            .try_get::<NaiveDateTime, _>(5)?
            .context("reporting row has a null DataTimeStamp")?;
        batch.push(ReportingRow {
            values: catalog_row_values(&row, columns)?,
            utc_micros: timestamp.and_utc().timestamp_micros(),
            data_item_id,
        });
        if batch.len() >= batch_size
            || max_rows.is_some_and(|limit| copied + batch.len() as u64 >= limit)
        {
            if let Some(limit) = max_rows {
                batch.truncate(limit.saturating_sub(copied) as usize);
            }
            if !batch.is_empty() {
                append_reporting_batch(target, point_id, &batch)?;
                copied = copied.saturating_add(batch.len() as u64);
                batch.clear();
            }
            if max_rows.is_some_and(|limit| copied >= limit) {
                return Ok(copied);
            }
        }
    }
    if !batch.is_empty() {
        append_reporting_batch(target, point_id, &batch)?;
        copied = copied.saturating_add(batch.len() as u64);
    }
    Ok(copied)
}

fn append_reporting_batch(
    target: &mut Connection,
    point_id: i32,
    batch: &[ReportingRow],
) -> Result<()> {
    let last = batch
        .last()
        .context("cannot append an empty reporting batch")?;
    let transaction = target.transaction()?;
    {
        let mut appender = transaction.appender_to_db(REPORTING_DATA_TABLE, REPORTING_SCHEMA)?;
        for row in batch {
            appender.append_row(appender_params_from_iter(row.values.iter()))?;
        }
        appender.flush()?;
    }
    transaction.execute(
        &format!(
            "INSERT INTO {CONTROL_SCHEMA}.reporting_checkpoints (point_id, last_utc_micros, last_data_item_id, rows_copied, updated_at) VALUES (?, ?, ?, ?, now()) ON CONFLICT (point_id) DO UPDATE SET last_utc_micros = excluded.last_utc_micros, last_data_item_id = excluded.last_data_item_id, rows_copied = {CONTROL_SCHEMA}.reporting_checkpoints.rows_copied + excluded.rows_copied, updated_at = now()"
        ),
        params![
            point_id,
            last.utc_micros,
            last.data_item_id,
            batch.len() as i64
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

fn reporting_checkpoint(target: &Connection, point_id: i32) -> Result<Option<(i64, i32)>> {
    target
        .query_row(
            &format!(
                "SELECT last_utc_micros, last_data_item_id FROM {CONTROL_SCHEMA}.reporting_checkpoints WHERE point_id = ?"
            ),
            [point_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .context("read DuckDB reporting checkpoint")
}

fn total_reporting_rows(target: &Connection) -> Result<u64> {
    let total = target.query_row(
        &format!(
            "SELECT COALESCE(SUM(rows_copied), 0)::BIGINT FROM {CONTROL_SCHEMA}.reporting_checkpoints"
        ),
        [],
        |row| row.get::<_, i64>(0),
    )?;
    u64::try_from(total).context("DuckDB reporting checkpoint total is negative")
}

fn validate_external_target(target: &Path, marker: &Path) -> Result<()> {
    if !target.is_absolute() || !marker.is_absolute() {
        bail!("DuckDB mirror target and volume marker must be absolute paths");
    }
    if target.extension().and_then(|value| value.to_str()) != Some("duckdb") {
        bail!("DuckDB mirror target must use the .duckdb extension");
    }
    let marker_metadata = fs::symlink_metadata(marker)
        .with_context(|| format!("external-volume marker is missing: {}", marker.display()))?;
    if !marker_metadata.file_type().is_file() || marker_metadata.file_type().is_symlink() {
        bail!("external-volume marker must be a regular file, not a symlink");
    }
    let contents = fs::read_to_string(marker)
        .with_context(|| format!("read external-volume marker {}", marker.display()))?;
    if contents != VOLUME_MARKER_CONTENT {
        bail!("external-volume marker has unexpected contents");
    }
    if target.exists() && fs::symlink_metadata(target)?.file_type().is_symlink() {
        bail!("DuckDB mirror target cannot be a symlink");
    }

    let marker_root = marker
        .parent()
        .context("external-volume marker has no parent")?
        .canonicalize()
        .context("canonicalize external-volume root")?;
    let mut existing_parent = target
        .parent()
        .context("DuckDB mirror target has no parent directory")?;
    while !existing_parent.exists() {
        existing_parent = existing_parent
            .parent()
            .context("DuckDB mirror target has no existing ancestor")?;
    }
    let existing_parent = existing_parent
        .canonicalize()
        .context("canonicalize DuckDB target ancestor")?;
    if !existing_parent.starts_with(&marker_root) {
        bail!(
            "DuckDB mirror target must be on marked external volume {}",
            marker_root.display()
        );
    }
    if existing_parent.metadata()?.dev() != marker_metadata.dev() {
        bail!("DuckDB mirror target and volume marker are on different filesystems");
    }
    Ok(())
}

fn initialize_target(target: &Connection) -> Result<()> {
    target
        .execute_batch(&format!(
            r#"
            PRAGMA threads=4;
            PRAGMA memory_limit='4GB';
            PRAGMA checkpoint_threshold='1GB';
            CREATE SCHEMA IF NOT EXISTS {TARGET_SCHEMA};
            CREATE SCHEMA IF NOT EXISTS {CONTROL_SCHEMA};
            CREATE TABLE IF NOT EXISTS {CONTROL_SCHEMA}.source_identity (
                singleton BOOLEAN PRIMARY KEY DEFAULT true CHECK (singleton),
                source_host VARCHAR NOT NULL,
                source_port INTEGER NOT NULL,
                source_database VARCHAR NOT NULL,
                created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
            );
            CREATE TABLE IF NOT EXISTS {CONTROL_SCHEMA}.source_schema_snapshots (
                captured_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
                schema_json VARCHAR NOT NULL
            );
            CREATE TABLE IF NOT EXISTS {CONTROL_SCHEMA}.database_schema_snapshots (
                source_database VARCHAR NOT NULL,
                captured_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
                schema_json VARCHAR NOT NULL
            );
            CREATE TABLE IF NOT EXISTS {CONTROL_SCHEMA}.source_database_mappings (
                source_database VARCHAR PRIMARY KEY,
                target_schema VARCHAR NOT NULL UNIQUE,
                updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
            );
            CREATE TABLE IF NOT EXISTS {CONTROL_SCHEMA}.event_checkpoints (
                table_name VARCHAR NOT NULL,
                point_slice_id INTEGER NOT NULL,
                last_utc_micros BIGINT NOT NULL,
                rows_copied BIGINT NOT NULL,
                updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
                PRIMARY KEY (table_name, point_slice_id)
            );
            CREATE TABLE IF NOT EXISTS {CONTROL_SCHEMA}.mirror_runs (
                run_id VARCHAR PRIMARY KEY,
                started_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
                finished_at TIMESTAMP,
                source_database VARCHAR NOT NULL,
                max_event_rows BIGINT,
                status VARCHAR NOT NULL,
                catalog_rows BIGINT,
                event_rows_this_run BIGINT,
                event_rows_total BIGINT,
                error_message VARCHAR
            );
            CREATE TABLE IF NOT EXISTS {CONTROL_SCHEMA}.full_passes (
                completed_at TIMESTAMP NOT NULL,
                source_event_rows_at_start BIGINT NOT NULL,
                target_event_rows BIGINT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS {CONTROL_SCHEMA}.reporting_checkpoints (
                point_id INTEGER NOT NULL PRIMARY KEY,
                last_utc_micros BIGINT NOT NULL,
                last_data_item_id INTEGER NOT NULL,
                rows_copied BIGINT NOT NULL,
                updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
            );
            CREATE TABLE IF NOT EXISTS {CONTROL_SCHEMA}.operational_passes (
                completed_at TIMESTAMP NOT NULL,
                database_count INTEGER NOT NULL,
                rows_copied_this_run BIGINT NOT NULL,
                reporting_source_rows_at_start BIGINT NOT NULL,
                reporting_target_rows BIGINT NOT NULL
            );
            UPDATE {CONTROL_SCHEMA}.mirror_runs
               SET status = 'interrupted', finished_at = current_timestamp
             WHERE status = 'running';
            "#
        ))
        .context("initialize DuckDB historian mirror schema")?;
    Ok(())
}

fn verify_source_identity(target: &Connection, source: &ConnectionSpec) -> Result<()> {
    let existing = target
        .query_row(
            &format!(
                "SELECT source_host, source_port, source_database FROM {CONTROL_SCHEMA}.source_identity WHERE singleton = true"
            ),
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i32>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    if let Some((host, port, database)) = existing {
        if host != source.host || port != i32::from(source.port) || database != source.database {
            bail!(
                "DuckDB target belongs to SQL source {host}:{port}/{database}, not {}:{}/{}",
                source.host,
                source.port,
                source.database
            );
        }
    } else {
        target.execute(
            &format!(
                "INSERT INTO {CONTROL_SCHEMA}.source_identity (singleton, source_host, source_port, source_database) VALUES (true, ?, ?, ?)"
            ),
            params![source.host, i32::from(source.port), source.database],
        )?;
    }
    Ok(())
}

fn record_schema_snapshot(
    target: &Connection,
    inspection: &HistorianDatabaseInspection,
) -> Result<()> {
    let encoded = serde_json::to_string(inspection).context("encode SQL source schema snapshot")?;
    target.execute(
        &format!(
            "INSERT INTO {CONTROL_SCHEMA}.source_schema_snapshots (captured_at, schema_json) SELECT current_timestamp, ? WHERE NOT EXISTS (SELECT 1 FROM {CONTROL_SCHEMA}.source_schema_snapshots WHERE schema_json = ?)"
        ),
        params![encoded, encoded],
    )?;
    Ok(())
}

fn begin_run(
    target: &Connection,
    source: &ConnectionSpec,
    max_event_rows: Option<u64>,
) -> Result<String> {
    let run_id = format!(
        "{}-{}",
        Utc::now().format("%Y%m%dT%H%M%S%.6fZ"),
        std::process::id()
    );
    let max_rows = max_event_rows
        .map(i64::try_from)
        .transpose()
        .context("max event row limit is too large")?;
    target.execute(
        &format!(
            "INSERT INTO {CONTROL_SCHEMA}.mirror_runs (run_id, source_database, max_event_rows, status) VALUES (?, ?, ?, 'running')"
        ),
        params![run_id, source.database, max_rows],
    )?;
    Ok(run_id)
}

fn finish_run(
    target: &Connection,
    run_id: &str,
    status: &str,
    error: Option<&str>,
    report: &MirrorReport,
) -> Result<()> {
    target.execute(
        &format!(
            "UPDATE {CONTROL_SCHEMA}.mirror_runs SET finished_at = current_timestamp, status = ?, catalog_rows = ?, event_rows_this_run = ?, event_rows_total = ?, error_message = ? WHERE run_id = ?"
        ),
        params![
            status,
            report.catalog_rows_copied as i64,
            report.event_rows_copied_this_run as i64,
            report.event_rows_copied_total as i64,
            error,
            run_id
        ],
    )?;
    Ok(())
}

fn finish_run_error(target: &Connection, run_id: &str, message: &str) -> Result<()> {
    target.execute(
        &format!(
            "UPDATE {CONTROL_SCHEMA}.mirror_runs SET finished_at = current_timestamp, status = 'failed', error_message = ? WHERE run_id = ?"
        ),
        params![message, run_id],
    )?;
    Ok(())
}

fn table_columns<'a>(
    inspection: &'a HistorianDatabaseInspection,
    table: &HistorianTableInfo,
) -> Vec<&'a HistorianColumnInfo> {
    let mut columns = inspection
        .columns
        .iter()
        .filter(|column| {
            column.table_schema == table.table_schema && column.table_name == table.table_name
        })
        .collect::<Vec<_>>();
    columns.sort_by_key(|column| column.ordinal_position);
    columns
}

async fn copy_catalog_table(
    source: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
    target: &mut Connection,
    target_schema: &str,
    table: &HistorianTableInfo,
    columns: &[&HistorianColumnInfo],
) -> Result<u64> {
    if columns.is_empty() {
        bail!(
            "SQL source table {}.{} has no columns",
            table.table_schema,
            table.table_name
        );
    }
    let staging_name = format!("{}__metasys_stage", table.table_name);
    let ddl = create_table_sql(target_schema, &staging_name, columns)?;
    target.execute_batch(&format!(
        "DROP TABLE IF EXISTS {}.{}; {ddl}",
        quote_duck_identifier(target_schema),
        quote_duck_identifier(&staging_name),
    ))?;

    let query = format!(
        "SELECT {} FROM {}.{}",
        columns
            .iter()
            .map(|column| quote_sql_server_identifier(&column.column_name))
            .collect::<Vec<_>>()
            .join(", "),
        quote_sql_server_identifier(&table.table_schema),
        quote_sql_server_identifier(&table.table_name),
    );
    let query = source
        .simple_query(query)
        .await
        .with_context(|| format!("read SQL source table {}", table.table_name))?;
    let mut rows = query.into_row_stream();
    let mut copied = 0u64;
    {
        let mut appender = target
            .appender_to_db(&staging_name, target_schema)
            .with_context(|| format!("open DuckDB appender for {staging_name}"))?;
        while let Some(row) = rows
            .try_next()
            .await
            .with_context(|| format!("stream SQL source table {}", table.table_name))?
        {
            let values = catalog_row_values(&row, columns)?;
            appender
                .append_row(appender_params_from_iter(values))
                .with_context(|| format!("append DuckDB row for {}", table.table_name))?;
            copied = copied.saturating_add(1);
        }
        appender.flush()?;
    }

    let transaction = target.transaction()?;
    transaction.execute_batch(&format!(
        "DROP TABLE IF EXISTS {}.{}; ALTER TABLE {}.{} RENAME TO {};",
        quote_duck_identifier(target_schema),
        quote_duck_identifier(&table.table_name),
        quote_duck_identifier(target_schema),
        quote_duck_identifier(&staging_name),
        quote_duck_identifier(&table.table_name),
    ))?;
    transaction.commit()?;
    Ok(copied)
}

fn create_table_sql(
    target_schema: &str,
    table_name: &str,
    columns: &[&HistorianColumnInfo],
) -> Result<String> {
    let definitions = columns
        .iter()
        .map(|column| {
            Ok(format!(
                "{} {}{}",
                quote_duck_identifier(&column.column_name),
                duck_type(column)?,
                if column.nullable { "" } else { " NOT NULL" }
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!(
        "CREATE TABLE {}.{} ({})",
        quote_duck_identifier(target_schema),
        quote_duck_identifier(table_name),
        definitions.join(", ")
    ))
}

fn duck_type(column: &HistorianColumnInfo) -> Result<String> {
    let data_type = column.data_type.to_ascii_lowercase();
    let mapped = match data_type.as_str() {
        "tinyint" => "UTINYINT".to_owned(),
        "smallint" => "SMALLINT".to_owned(),
        "int" => "INTEGER".to_owned(),
        "bigint" => "BIGINT".to_owned(),
        "real" => "FLOAT".to_owned(),
        "float" => "DOUBLE".to_owned(),
        "bit" => "BOOLEAN".to_owned(),
        "decimal" | "numeric" => {
            format!("DECIMAL({}, {})", column.precision, column.scale)
        }
        "money" => "DECIMAL(19, 4)".to_owned(),
        "smallmoney" => "DECIMAL(10, 4)".to_owned(),
        "date" => "DATE".to_owned(),
        "time" => "TIME".to_owned(),
        "datetime" | "datetime2" | "smalldatetime" => "TIMESTAMP".to_owned(),
        "datetimeoffset" => "TIMESTAMPTZ".to_owned(),
        "binary" | "varbinary" | "image" => "BLOB".to_owned(),
        "char" | "nchar" | "varchar" | "nvarchar" | "text" | "ntext" | "xml"
        | "uniqueidentifier" => "VARCHAR".to_owned(),
        _ => bail!(
            "unsupported SQL source type {} for {}.{}",
            column.data_type,
            column.table_name,
            column.column_name
        ),
    };
    Ok(mapped)
}

fn catalog_row_values(row: &Row, columns: &[&HistorianColumnInfo]) -> Result<Vec<Value>> {
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| source_value(row, index, column))
        .collect()
}

fn source_value(row: &Row, index: usize, column: &HistorianColumnInfo) -> Result<Value> {
    let kind = column.data_type.to_ascii_lowercase();
    let value = match kind.as_str() {
        "tinyint" => optional_value(row.try_get::<u8, _>(index)?, Value::UTinyInt),
        "smallint" => optional_value(row.try_get::<i16, _>(index)?, Value::SmallInt),
        "int" => optional_value(row.try_get::<i32, _>(index)?, Value::Int),
        "bigint" => optional_value(row.try_get::<i64, _>(index)?, Value::BigInt),
        "real" => optional_value(row.try_get::<f32, _>(index)?, Value::Float),
        "float" => optional_value(row.try_get::<f64, _>(index)?, Value::Double),
        "bit" => optional_value(row.try_get::<bool, _>(index)?, Value::Boolean),
        "decimal" | "numeric" | "money" | "smallmoney" => {
            match row.try_get::<tiberius::numeric::Numeric, _>(index)? {
                Some(value) => Value::Decimal(
                    Decimal::new(column.precision, column.scale, value.value()).with_context(
                        || {
                            format!(
                                "convert decimal column {}.{}",
                                column.table_name, column.column_name
                            )
                        },
                    )?,
                ),
                None => Value::Null,
            }
        }
        "datetime" | "datetime2" | "smalldatetime" => {
            optional_value(row.try_get::<NaiveDateTime, _>(index)?, timestamp_value)
        }
        "date" => optional_value(row.try_get::<NaiveDate, _>(index)?, |date| {
            Value::Date32(
                date.signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
                    .num_days() as i32,
            )
        }),
        "time" => optional_value(row.try_get::<NaiveTime, _>(index)?, |time| {
            Value::Time64(
                TimeUnit::Microsecond,
                i64::from(time.num_seconds_from_midnight()) * 1_000_000
                    + i64::from(time.nanosecond() / 1_000),
            )
        }),
        "char" | "nchar" | "varchar" | "nvarchar" | "text" | "ntext" => {
            optional_value(row.try_get::<&str, _>(index)?, |text| {
                Value::Text(text.to_owned())
            })
        }
        "binary" | "varbinary" | "image" => {
            optional_value(row.try_get::<&[u8], _>(index)?, |bytes| {
                Value::Blob(bytes.to_vec())
            })
        }
        "uniqueidentifier" => optional_value(row.try_get::<tiberius::Uuid, _>(index)?, |id| {
            Value::Text(id.to_string())
        }),
        "xml" => optional_value(row.try_get::<&tiberius::xml::XmlData, _>(index)?, |xml| {
            Value::Text(xml.as_ref().to_owned())
        }),
        "datetimeoffset" => {
            bail!(
                "SQL source type {} requires an explicit converter for {}.{}",
                column.data_type,
                column.table_name,
                column.column_name
            )
        }
        _ => bail!(
            "unsupported SQL source type {} for {}.{}",
            column.data_type,
            column.table_name,
            column.column_name
        ),
    };
    Ok(value)
}

fn optional_value<T>(value: Option<T>, convert: impl FnOnce(T) -> Value) -> Value {
    value.map(convert).unwrap_or(Value::Null)
}

fn timestamp_value(value: NaiveDateTime) -> Value {
    Value::Timestamp(TimeUnit::Microsecond, value.and_utc().timestamp_micros())
}

fn ensure_event_tables(
    target: &Connection,
    inspection: &HistorianDatabaseInspection,
) -> Result<()> {
    for name in EVENT_TABLES {
        let table = inspection
            .tables
            .iter()
            .find(|table| table.table_name == name)
            .with_context(|| format!("SQL source event table {name} is missing"))?;
        let columns = table_columns(inspection, table);
        let ddl = create_table_sql(TARGET_SCHEMA, name, &columns)?;
        target.execute_batch(&ddl.replace("CREATE TABLE", "CREATE TABLE IF NOT EXISTS"))?;
    }
    Ok(())
}

fn read_point_slice_ids(target: &Connection) -> Result<Vec<i32>> {
    let mut statement = target.prepare(&format!(
        "SELECT PointSliceID FROM {TARGET_SCHEMA}.tblPointSlice ORDER BY PointSliceID"
    ))?;
    let rows = statement.query_map([], |row| row.get::<_, i32>(0))?;
    rows.collect::<duckdb::Result<Vec<_>>>()
        .context("read mirrored point-slice catalog")
}

async fn copy_event_point(
    source: &mut tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>,
    target: &mut Connection,
    kind: EventKind,
    point_slice_id: i32,
    batch_size: usize,
    max_rows: Option<u64>,
) -> Result<u64> {
    let cursor = event_checkpoint(target, kind.table_name(), point_slice_id)?;
    let query = if cursor.is_some() {
        format!(
            "SELECT {} FROM [dbo].[{}] WHERE [PointSliceID] = @P1 AND [UTCDateTime] > @P2 ORDER BY [UTCDateTime] ASC",
            kind.select_columns(),
            kind.table_name()
        )
    } else {
        format!(
            "SELECT {} FROM [dbo].[{}] WHERE [PointSliceID] = @P1 ORDER BY [UTCDateTime] ASC",
            kind.select_columns(),
            kind.table_name()
        )
    };
    let query = if let Some(cursor) = cursor {
        let timestamp = DateTime::<Utc>::from_timestamp_micros(cursor)
            .context("DuckDB event checkpoint is outside the supported timestamp range")?
            .naive_utc();
        source.query(query, &[&point_slice_id, &timestamp]).await?
    } else {
        source.query(query, &[&point_slice_id]).await?
    };
    let mut stream = query.into_row_stream();
    let mut batch = Vec::with_capacity(batch_size);
    let mut copied = 0u64;
    while let Some(row) = stream
        .try_next()
        .await
        .with_context(|| format!("stream {} point {point_slice_id}", kind.table_name()))?
    {
        batch.push(decode_event_row(kind, &row)?);
        if batch.len() >= batch_size
            || max_rows.is_some_and(|limit| copied + batch.len() as u64 >= limit)
        {
            if let Some(limit) = max_rows {
                batch.truncate(limit.saturating_sub(copied) as usize);
            }
            if !batch.is_empty() {
                append_event_batch(target, kind, point_slice_id, &batch)?;
                copied = copied.saturating_add(batch.len() as u64);
                batch.clear();
            }
            if max_rows.is_some_and(|limit| copied >= limit) {
                return Ok(copied);
            }
        }
    }
    if !batch.is_empty() {
        append_event_batch(target, kind, point_slice_id, &batch)?;
        copied = copied.saturating_add(batch.len() as u64);
    }
    Ok(copied)
}

fn decode_event_row(kind: EventKind, row: &Row) -> Result<EventRow> {
    let point_slice_id = row
        .try_get::<i32, _>(0)?
        .context("event row has a null PointSliceID")?;
    let timestamp = row
        .try_get::<NaiveDateTime, _>(1)?
        .context("event row has a null UTCDateTime")?;
    let value = match kind {
        EventKind::ActualFloat | EventKind::OtherFloat => EventValue::Float(
            row.try_get::<f32, _>(2)?
                .context("event row has a null float value")?,
        ),
        EventKind::ActualDigital | EventKind::OtherDigital => EventValue::Digital(
            row.try_get::<i16, _>(2)?
                .context("event row has a null digital value")?,
        ),
    };
    let (value_category_id, status) = match kind {
        EventKind::OtherFloat | EventKind::OtherDigital => (
            Some(
                row.try_get::<i32, _>(3)?
                    .context("other-value event row has a null ValueCategoryID")?,
            ),
            row.try_get::<i32, _>(4)?,
        ),
        EventKind::ActualFloat | EventKind::ActualDigital => (None, None),
    };
    Ok(EventRow {
        point_slice_id,
        utc_micros: timestamp.and_utc().timestamp_micros(),
        value,
        value_category_id,
        status,
    })
}

fn append_event_batch(
    target: &mut Connection,
    kind: EventKind,
    point_slice_id: i32,
    batch: &[EventRow],
) -> Result<()> {
    let last_micros = batch
        .last()
        .context("cannot append an empty event batch")?
        .utc_micros;
    let transaction = target.transaction()?;
    {
        let mut appender = transaction.appender_to_db(kind.table_name(), TARGET_SCHEMA)?;
        for row in batch {
            if row.point_slice_id != point_slice_id {
                bail!("event batch contains a mismatched PointSliceID");
            }
            let timestamp = Value::Timestamp(TimeUnit::Microsecond, row.utc_micros);
            let values = match (&row.value, row.value_category_id) {
                (EventValue::Float(value), None) => vec![
                    Value::Int(row.point_slice_id),
                    timestamp,
                    Value::Float(*value),
                ],
                (EventValue::Digital(value), None) => vec![
                    Value::Int(row.point_slice_id),
                    timestamp,
                    Value::SmallInt(*value),
                ],
                (EventValue::Float(value), Some(category)) => vec![
                    Value::Int(row.point_slice_id),
                    timestamp,
                    Value::Float(*value),
                    Value::Int(category),
                    row.status.map(Value::Int).unwrap_or(Value::Null),
                ],
                (EventValue::Digital(value), Some(category)) => vec![
                    Value::Int(row.point_slice_id),
                    timestamp,
                    Value::SmallInt(*value),
                    Value::Int(category),
                    row.status.map(Value::Int).unwrap_or(Value::Null),
                ],
            };
            appender.append_row(appender_params_from_iter(values))?;
        }
        appender.flush()?;
    }
    transaction.execute(
        &format!(
            "INSERT INTO {CONTROL_SCHEMA}.event_checkpoints (table_name, point_slice_id, last_utc_micros, rows_copied, updated_at) VALUES (?, ?, ?, ?, now()) ON CONFLICT (table_name, point_slice_id) DO UPDATE SET last_utc_micros = excluded.last_utc_micros, rows_copied = {CONTROL_SCHEMA}.event_checkpoints.rows_copied + excluded.rows_copied, updated_at = now()"
        ),
        params![
            kind.table_name(),
            point_slice_id,
            last_micros,
            batch.len() as i64
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

fn event_checkpoint(
    target: &Connection,
    table_name: &str,
    point_slice_id: i32,
) -> Result<Option<i64>> {
    target
        .query_row(
            &format!(
                "SELECT last_utc_micros FROM {CONTROL_SCHEMA}.event_checkpoints WHERE table_name = ? AND point_slice_id = ?"
            ),
            params![table_name, point_slice_id],
            |row| row.get(0),
        )
        .optional()
        .context("read DuckDB event checkpoint")
}

fn total_event_rows(target: &Connection) -> Result<u64> {
    let total = target.query_row(
        &format!(
            "SELECT COALESCE(SUM(rows_copied), 0)::BIGINT FROM {CONTROL_SCHEMA}.event_checkpoints"
        ),
        [],
        |row| row.get::<_, i64>(0),
    )?;
    u64::try_from(total).context("DuckDB event checkpoint total is negative")
}

fn quote_sql_server_identifier(value: &str) -> String {
    format!("[{}]", value.replace(']', "]]"))
}

fn quote_duck_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use duckdb::Connection;
    use tempfile::tempdir;

    use super::{
        append_event_batch, append_reporting_batch, initialize_target, register_database_mapping,
        total_event_rows, total_reporting_rows, validate_external_target, EventKind, EventRow,
        EventValue, ReportingRow, TimeUnit, Value, VOLUME_MARKER_CONTENT,
    };

    #[test]
    fn external_target_requires_the_marker_and_same_volume() {
        let directory = tempdir().unwrap();
        let marker = directory.path().join(".marker");
        let target = directory.path().join("data/history.duckdb");

        assert!(validate_external_target(&target, &marker).is_err());
        fs::write(&marker, VOLUME_MARKER_CONTENT).unwrap();
        validate_external_target(&target, &marker).unwrap();
        fs::write(&marker, "wrong marker\n").unwrap();
        assert!(validate_external_target(&target, &marker).is_err());
    }

    #[test]
    fn source_database_mapping_is_idempotent_and_updateable() {
        let target = Connection::open_in_memory().unwrap();
        initialize_target(&target).unwrap();

        register_database_mapping(&target, "Example", "sql_server__Example").unwrap();
        register_database_mapping(&target, "Example", "sql_server__Example").unwrap();
        register_database_mapping(&target, "Example", "sql_server__Example_v2").unwrap();

        let mapping: (i64, String) = target
            .query_row(
                "SELECT COUNT(*)::BIGINT, min(target_schema) FROM metasys_migration.source_database_mappings WHERE source_database = 'Example'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(mapping, (1, "sql_server__Example_v2".to_owned()));
    }

    #[test]
    fn all_event_shapes_commit_with_their_checkpoints_and_resume() {
        let mut target = Connection::open_in_memory().unwrap();
        initialize_target(&target).unwrap();
        target
            .execute_batch(
                r#"
                CREATE TABLE jci_historian.tblActualValueFloat (PointSliceID INTEGER NOT NULL, UTCDateTime TIMESTAMP NOT NULL, ActualValue FLOAT NOT NULL);
                CREATE TABLE jci_historian.tblActualValueDigital (PointSliceID INTEGER NOT NULL, UTCDateTime TIMESTAMP NOT NULL, ActualValue SMALLINT NOT NULL);
                CREATE TABLE jci_historian.tblOtherValueFloat (PointSliceID INTEGER NOT NULL, UTCDateTime TIMESTAMP NOT NULL, OtherValue FLOAT NOT NULL, ValueCategoryID INTEGER NOT NULL, Status INTEGER);
                CREATE TABLE jci_historian.tblOtherValueDigital (PointSliceID INTEGER NOT NULL, UTCDateTime TIMESTAMP NOT NULL, OtherValue SMALLINT NOT NULL, ValueCategoryID INTEGER NOT NULL, Status INTEGER);
                "#,
            )
            .unwrap();

        append_event_batch(
            &mut target,
            EventKind::ActualFloat,
            7,
            &[EventRow {
                point_slice_id: 7,
                utc_micros: 1_700_000_000_000_000,
                value: EventValue::Float(12.5),
                value_category_id: None,
                status: None,
            }],
        )
        .unwrap();
        append_event_batch(
            &mut target,
            EventKind::ActualDigital,
            8,
            &[EventRow {
                point_slice_id: 8,
                utc_micros: 1_700_000_002_000_000,
                value: EventValue::Digital(3),
                value_category_id: None,
                status: None,
            }],
        )
        .unwrap();
        append_event_batch(
            &mut target,
            EventKind::OtherFloat,
            9,
            &[EventRow {
                point_slice_id: 9,
                utc_micros: 1_700_000_003_000_000,
                value: EventValue::Float(22.25),
                value_category_id: Some(2),
                status: Some(64),
            }],
        )
        .unwrap();
        append_event_batch(
            &mut target,
            EventKind::OtherDigital,
            10,
            &[EventRow {
                point_slice_id: 10,
                utc_micros: 1_700_000_004_000_000,
                value: EventValue::Digital(1),
                value_category_id: Some(1),
                status: None,
            }],
        )
        .unwrap();
        append_event_batch(
            &mut target,
            EventKind::ActualFloat,
            7,
            &[EventRow {
                point_slice_id: 7,
                utc_micros: 1_700_000_001_000_000,
                value: EventValue::Float(13.5),
                value_category_id: None,
                status: None,
            }],
        )
        .unwrap();

        let row_count: i64 = target
            .query_row(
                "SELECT COUNT(*) FROM jci_historian.tblActualValueFloat",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let checkpoint: (i64, i64) = target
            .query_row(
                "SELECT last_utc_micros, rows_copied FROM metasys_migration.event_checkpoints WHERE table_name = 'tblActualValueFloat' AND point_slice_id = 7",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row_count, 2);
        assert_eq!(checkpoint, (1_700_000_001_000_000, 2));
        let digital_value: i16 = target
            .query_row(
                "SELECT ActualValue FROM jci_historian.tblActualValueDigital",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let other_float: (f32, i32, Option<i32>) = target
            .query_row(
                "SELECT OtherValue, ValueCategoryID, Status FROM jci_historian.tblOtherValueFloat",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let other_digital: (i16, i32, Option<i32>) = target
            .query_row(
                "SELECT OtherValue, ValueCategoryID, Status FROM jci_historian.tblOtherValueDigital",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(digital_value, 3);
        assert_eq!(other_float, (22.25, 2, Some(64)));
        assert_eq!(other_digital, (1, 1, None));
        assert_eq!(total_event_rows(&target).unwrap(), 5);
    }

    #[test]
    fn reporting_batches_use_timestamp_and_identity_as_a_resumable_cursor() {
        let mut target = Connection::open_in_memory().unwrap();
        initialize_target(&target).unwrap();
        target
            .execute_batch(
                "CREATE SCHEMA jci_reporting; CREATE TABLE jci_reporting.tblDataItem (DataItemID INTEGER, PointID INTEGER, SourceTypeID INTEGER, Year INTEGER, Month INTEGER, DataTimeStamp TIMESTAMP, Value FLOAT)",
            )
            .unwrap();
        let row = |id, micros, value| ReportingRow {
            values: vec![
                Value::Int(id),
                Value::Int(7),
                Value::Int(1),
                Value::Int(2026),
                Value::Int(8),
                Value::Timestamp(TimeUnit::Microsecond, micros),
                Value::Float(value),
            ],
            utc_micros: micros,
            data_item_id: id,
        };
        append_reporting_batch(&mut target, 7, &[row(10, 1_700_000_000_000_000, 1.5)]).unwrap();
        append_reporting_batch(&mut target, 7, &[row(11, 1_700_000_000_000_000, 2.5)]).unwrap();

        let checkpoint: (i64, i32, i64) = target
            .query_row(
                "SELECT last_utc_micros, last_data_item_id, rows_copied FROM metasys_migration.reporting_checkpoints WHERE point_id = 7",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(checkpoint, (1_700_000_000_000_000, 11, 2));
        assert_eq!(total_reporting_rows(&target).unwrap(), 2);
    }
}
