use std::{
    fs,
    path::{Component, Path},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use duckdb::{AccessMode, Config as DuckDbConfig, Connection, OptionalExt};
use serde::{Deserialize, Serialize};

pub const SQL_MIRROR_LAUNCH_AGENT_LABEL: &str = "io.github.metasys-summary.sql-history-mirror";
pub const DEFAULT_SQL_MIRROR_TARGET: &str = "/Volumes/TestStorage/MetasysData/JCIHistorian.duckdb";
pub const DEFAULT_SQL_MIRROR_VOLUME_MARKER: &str = "/Volumes/TestStorage/.metasys-storage-volume";
const SQL_MIRROR_VOLUME_MARKER_CONTENT: &str = "METASYS_SUMMARY_EXTERNAL_STORAGE_V1\n";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlMirrorSettings {
    pub enabled: bool,
    pub target_database: String,
    pub volume_marker: String,
    pub interval_hours: u16,
    pub batch_size: usize,
}

impl Default for SqlMirrorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            target_database: DEFAULT_SQL_MIRROR_TARGET.to_owned(),
            volume_marker: DEFAULT_SQL_MIRROR_VOLUME_MARKER.to_owned(),
            interval_hours: 1,
            batch_size: 250_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlMirrorSettingsUpdate {
    pub enabled: bool,
    pub target_database: String,
    pub volume_marker: String,
    pub interval_hours: u16,
    pub batch_size: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlMirrorRunRecord {
    pub run_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub target_database: String,
    pub volume_marker: String,
    pub duration_ms: Option<u64>,
    pub event_rows_copied: Option<u64>,
    pub event_rows_total: Option<u64>,
    pub source_event_rows: Option<u64>,
    pub total_mirrored_rows: Option<u64>,
    pub integrity_ok: Option<bool>,
    pub error_message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlMirrorHealthView {
    pub state: String,
    pub message: String,
    pub scheduler_loaded: bool,
    pub volume_marker_present: bool,
    pub target_database_present: bool,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub next_due_at: Option<DateTime<Utc>>,
    pub consecutive_failures: usize,
    pub last_error: Option<String>,
    pub last_duration_seconds: Option<f64>,
    pub event_rows_copied_last_run: Option<u64>,
    pub event_rows_total: Option<u64>,
    pub source_event_rows: Option<u64>,
    pub total_mirrored_rows: Option<u64>,
    pub integrity_ok: Option<bool>,
    pub recent_runs: Vec<SqlMirrorRunRecord>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlMirrorSettingsView {
    pub enabled: bool,
    pub target_database: String,
    pub volume_marker: String,
    pub interval_hours: u16,
    pub batch_size: usize,
    pub health: SqlMirrorHealthView,
}

impl SqlMirrorSettingsUpdate {
    pub fn validated_settings(&self) -> Result<SqlMirrorSettings> {
        let settings = SqlMirrorSettings {
            enabled: self.enabled,
            target_database: self.target_database.trim().to_owned(),
            volume_marker: self.volume_marker.trim().to_owned(),
            interval_hours: self.interval_hours,
            batch_size: self.batch_size,
        };
        settings.validate()?;
        Ok(settings)
    }
}

impl SqlMirrorSettings {
    pub fn validate(&self) -> Result<()> {
        if !(1..=168).contains(&self.interval_hours) {
            bail!("mirror cadence must be between 1 and 168 hours");
        }
        if !(10_000..=1_000_000).contains(&self.batch_size) {
            bail!("mirror batch size must be between 10,000 and 1,000,000 rows");
        }
        let target = validate_configured_path("DuckDB mirror target", &self.target_database)?;
        let marker = validate_configured_path("external-volume marker", &self.volume_marker)?;
        if target.extension().and_then(|value| value.to_str()) != Some("duckdb") {
            bail!("DuckDB mirror target must end in .duckdb");
        }
        let marker_root = marker
            .parent()
            .context("external-volume marker must have a parent directory")?;
        if !target.starts_with(marker_root) || target == marker {
            bail!("DuckDB mirror target must be beneath the marker's volume directory");
        }
        Ok(())
    }

    pub fn is_due(&self, now: DateTime<Utc>, latest: Option<&SqlMirrorRunRecord>) -> bool {
        if !self.enabled || latest.is_some_and(|run| run.status == "running") {
            return false;
        }
        if latest.is_some_and(|run| !run.matches_settings(self)) {
            return true;
        }
        latest.is_none_or(|run| {
            now >= run.started_at + Duration::hours(i64::from(self.interval_hours))
        })
    }

    pub fn view(
        &self,
        recent_runs: Vec<SqlMirrorRunRecord>,
        now: DateTime<Utc>,
        scheduler_loaded: bool,
    ) -> SqlMirrorSettingsView {
        let volume_marker_present = Path::new(&self.volume_marker).is_file();
        let target_database_present = Path::new(&self.target_database).is_file();
        let current_runs = recent_runs
            .iter()
            .filter(|run| run.matches_settings(self))
            .collect::<Vec<_>>();
        let latest = current_runs.first().copied();
        let last_success = current_runs
            .iter()
            .copied()
            .find(|run| run.status == "succeeded");
        let next_due_at =
            latest.map(|run| run.started_at + Duration::hours(i64::from(self.interval_hours)));
        let consecutive_failures = current_runs
            .iter()
            .take_while(|run| matches!(run.status.as_str(), "failed" | "interrupted"))
            .count();
        let latest_metrics = last_success.or(latest);
        let (state, message) = mirror_health_state(
            self,
            latest,
            last_success,
            next_due_at,
            now,
            scheduler_loaded,
            volume_marker_present,
            target_database_present,
        );
        SqlMirrorSettingsView {
            enabled: self.enabled,
            target_database: self.target_database.clone(),
            volume_marker: self.volume_marker.clone(),
            interval_hours: self.interval_hours,
            batch_size: self.batch_size,
            health: SqlMirrorHealthView {
                state,
                message,
                scheduler_loaded,
                volume_marker_present,
                target_database_present,
                last_attempt_at: latest.map(|run| run.started_at),
                last_success_at: last_success
                    .and_then(|run| run.finished_at)
                    .or_else(|| last_success.map(|run| run.started_at)),
                next_due_at,
                consecutive_failures,
                last_error: latest.and_then(|run| run.error_message.clone()),
                last_duration_seconds: latest
                    .and_then(|run| run.duration_ms)
                    .map(|milliseconds| milliseconds as f64 / 1_000.0),
                event_rows_copied_last_run: latest.and_then(|run| run.event_rows_copied),
                event_rows_total: latest_metrics.and_then(|run| run.event_rows_total),
                source_event_rows: latest_metrics.and_then(|run| run.source_event_rows),
                total_mirrored_rows: latest_metrics.and_then(|run| run.total_mirrored_rows),
                integrity_ok: latest_metrics.and_then(|run| run.integrity_ok),
                recent_runs,
            },
        }
    }
}

impl SqlMirrorRunRecord {
    fn matches_settings(&self, settings: &SqlMirrorSettings) -> bool {
        self.target_database == settings.target_database
            && self.volume_marker == settings.volume_marker
    }
}

fn validate_configured_path<'a>(label: &str, value: &'a str) -> Result<&'a Path> {
    if value.is_empty() || value.len() > 1_024 || value.chars().any(char::is_control) {
        bail!("{label} must contain 1 to 1,024 non-control characters");
    }
    let path = Path::new(value);
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        bail!("{label} must be an absolute path without . or .. components");
    }
    Ok(path)
}

#[allow(clippy::too_many_arguments)]
fn mirror_health_state(
    settings: &SqlMirrorSettings,
    latest: Option<&SqlMirrorRunRecord>,
    last_success: Option<&SqlMirrorRunRecord>,
    next_due_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    scheduler_loaded: bool,
    volume_marker_present: bool,
    target_database_present: bool,
) -> (String, String) {
    if !settings.enabled {
        return (
            "disabled".to_owned(),
            "Scheduled SQL mirroring is disabled".to_owned(),
        );
    }
    if latest.is_some_and(|run| run.status == "running") {
        return (
            "running".to_owned(),
            "SQL mirror cycle is running".to_owned(),
        );
    }
    if !scheduler_loaded {
        return (
            "error".to_owned(),
            "Hourly SQL mirror LaunchAgent is not loaded".to_owned(),
        );
    }
    if !volume_marker_present {
        return (
            "error".to_owned(),
            "External mirror volume or marker is unavailable".to_owned(),
        );
    }
    if latest.is_some_and(|run| matches!(run.status.as_str(), "failed" | "interrupted")) {
        return (
            "error".to_owned(),
            latest
                .and_then(|run| run.error_message.clone())
                .unwrap_or_else(|| "Latest SQL mirror cycle failed".to_owned()),
        );
    }
    if last_success.is_none() {
        return (
            "neverRun".to_owned(),
            "No completed scheduled SQL mirror cycle has been recorded".to_owned(),
        );
    }
    if !target_database_present {
        return (
            "error".to_owned(),
            "Configured DuckDB mirror target is unavailable".to_owned(),
        );
    }
    if last_success.and_then(|run| run.integrity_ok) == Some(false) {
        return (
            "error".to_owned(),
            "Latest SQL mirror integrity check failed".to_owned(),
        );
    }
    if next_due_at.is_some_and(|due| now > due + Duration::minutes(15)) {
        return (
            "overdue".to_owned(),
            "SQL mirror is overdue for its configured cadence".to_owned(),
        );
    }
    (
        "healthy".to_owned(),
        "SQL mirror is current and its latest integrity check passed".to_owned(),
    )
}

pub fn sql_mirror_scheduler_loaded() -> bool {
    let uid = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_owned());
    let Some(uid) = uid.filter(|value| !value.is_empty()) else {
        return false;
    };
    Command::new("/bin/launchctl")
        .arg("print")
        .arg(format!("gui/{uid}/{SQL_MIRROR_LAUNCH_AGENT_LABEL}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

const EVENT_TABLES: [&str; 4] = [
    "tblActualValueFloat",
    "tblActualValueDigital",
    "tblOtherValueFloat",
    "tblOtherValueDigital",
];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlMirrorTableStatus {
    pub table_name: String,
    pub stored_rows: u64,
    pub checkpoint_rows: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlMirrorSnapshotMismatch {
    pub source_database: String,
    pub table_schema: String,
    pub table_name: String,
    pub expected_rows_at_start: u64,
    pub stored_rows: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceDatabaseSnapshot {
    tables: Vec<SourceTableSnapshot>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceTableSnapshot {
    table_schema: String,
    table_name: String,
    row_count: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqlMirrorStatus {
    pub target_database: String,
    pub database_bytes: u64,
    pub source_host: String,
    pub source_port: u16,
    pub source_database: String,
    pub mirrored_source_database_count: u64,
    pub mirrored_table_count: u64,
    pub historian_catalog_rows: u64,
    pub event_rows: u64,
    pub checkpoint_rows: u64,
    pub checkpoints_match_storage: bool,
    pub completed_full_passes: u64,
    pub operational_mirrored_table_count: u64,
    pub operational_rows: u64,
    pub operational_expected_rows_at_start: u64,
    pub operational_snapshot_counts_cover_source: bool,
    pub operational_snapshot_mismatches: Vec<SqlMirrorSnapshotMismatch>,
    pub total_mirrored_rows: u64,
    pub completed_operational_passes: u64,
    pub reporting_rows: u64,
    pub reporting_checkpoint_rows: u64,
    pub reporting_checkpoints_match_storage: bool,
    pub last_run_status: Option<String>,
    pub last_run_started_at: Option<String>,
    pub last_run_finished_at: Option<String>,
    pub tables: Vec<SqlMirrorTableStatus>,
}

pub fn inspect_sql_mirror(path: &Path) -> Result<SqlMirrorStatus> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("read DuckDB mirror metadata {}", path.display()))?;
    let config = DuckDbConfig::default()
        .access_mode(AccessMode::ReadOnly)
        .context("configure read-only DuckDB mirror check")?;
    let connection = Connection::open_with_flags(path, config)
        .with_context(|| format!("open DuckDB mirror read-only {}", path.display()))?;
    let (source_host, source_port, source_database) = connection.query_row(
        "SELECT source_host, source_port, source_database FROM metasys_migration.source_identity WHERE singleton = true",
        [],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i32>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )?;

    let mirrored_table_count = nonnegative_u64(connection.query_row(
        "SELECT COUNT(*)::BIGINT FROM information_schema.tables WHERE table_schema = 'jci_historian' AND table_type = 'BASE TABLE'",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    let historian_catalog_rows =
        schema_rows_excluding(&connection, "jci_historian", &EVENT_TABLES)?;
    let mut tables = Vec::new();
    for table_name in EVENT_TABLES {
        let stored_rows = nonnegative_u64(connection.query_row(
            &format!("SELECT COUNT(*)::BIGINT FROM jci_historian.{table_name}"),
            [],
            |row| row.get::<_, i64>(0),
        )?)?;
        let checkpoint_rows = nonnegative_u64(connection.query_row(
            "SELECT COALESCE(SUM(rows_copied), 0)::BIGINT FROM metasys_migration.event_checkpoints WHERE table_name = ?",
            [table_name],
            |row| row.get::<_, i64>(0),
        )?)?;
        tables.push(SqlMirrorTableStatus {
            table_name: table_name.to_owned(),
            stored_rows,
            checkpoint_rows,
        });
    }
    let event_rows = tables.iter().map(|table| table.stored_rows).sum();
    let checkpoint_rows = tables.iter().map(|table| table.checkpoint_rows).sum();
    let completed_full_passes = nonnegative_u64(connection.query_row(
        "SELECT COUNT(*)::BIGINT FROM metasys_migration.full_passes",
        [],
        |row| row.get::<_, i64>(0),
    )?)?;
    let mapping_table_exists =
        table_exists(&connection, "metasys_migration", "source_database_mappings")?;
    let mirrored_source_database_count = if mapping_table_exists {
        1 + nonnegative_u64(connection.query_row(
            "SELECT COUNT(*)::BIGINT FROM metasys_migration.source_database_mappings",
            [],
            |row| row.get::<_, i64>(0),
        )?)?
    } else {
        1
    };
    let operational_mirrored_table_count = if mapping_table_exists {
        nonnegative_u64(connection.query_row(
            "SELECT COUNT(*)::BIGINT FROM information_schema.tables AS source_tables JOIN metasys_migration.source_database_mappings AS mappings ON mappings.target_schema = source_tables.table_schema WHERE source_tables.table_type = 'BASE TABLE'",
            [],
            |row| row.get::<_, i64>(0),
        )?)?
    } else {
        nonnegative_u64(connection.query_row(
            "SELECT COUNT(*)::BIGINT FROM information_schema.tables WHERE table_schema IN ('jci_reporting', 'jci_audit_trails', 'jci_events', 'jci_item_annotation', 'metasys_reporting') AND table_type = 'BASE TABLE'",
            [],
            |row| row.get::<_, i64>(0),
        )?)?
    };
    let (
        operational_expected_table_count,
        operational_expected_rows_at_start,
        operational_rows,
        operational_snapshot_mismatches,
    ) = if mapping_table_exists {
        audit_operational_snapshots(&connection)?
    } else {
        (0, 0, 0, Vec::new())
    };
    let operational_snapshot_counts_cover_source = operational_snapshot_mismatches.is_empty()
        && operational_mirrored_table_count == operational_expected_table_count;
    let operational_control_exists =
        table_exists(&connection, "metasys_migration", "operational_passes")?;
    let completed_operational_passes = if operational_control_exists {
        nonnegative_u64(connection.query_row(
            "SELECT COUNT(*)::BIGINT FROM metasys_migration.operational_passes",
            [],
            |row| row.get::<_, i64>(0),
        )?)?
    } else {
        0
    };
    let reporting_table_exists = table_exists(&connection, "jci_reporting", "tblDataItem")?;
    let reporting_rows = if reporting_table_exists {
        nonnegative_u64(connection.query_row(
            "SELECT COUNT(*)::BIGINT FROM jci_reporting.tblDataItem",
            [],
            |row| row.get::<_, i64>(0),
        )?)?
    } else {
        0
    };
    let reporting_checkpoint_rows = if table_exists(
        &connection,
        "metasys_migration",
        "reporting_checkpoints",
    )? {
        nonnegative_u64(connection.query_row(
            "SELECT COALESCE(SUM(rows_copied), 0)::BIGINT FROM metasys_migration.reporting_checkpoints",
            [],
            |row| row.get::<_, i64>(0),
        )?)?
    } else {
        0
    };
    let last_run = connection
        .query_row(
            "SELECT status, CAST(started_at AS VARCHAR), CAST(finished_at AS VARCHAR) FROM metasys_migration.mirror_runs ORDER BY started_at DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok();

    Ok(SqlMirrorStatus {
        target_database: path.display().to_string(),
        database_bytes: metadata.len(),
        source_host,
        source_port: u16::try_from(source_port).context("stored SQL source port is invalid")?,
        source_database,
        mirrored_source_database_count,
        mirrored_table_count,
        historian_catalog_rows,
        event_rows,
        checkpoint_rows,
        checkpoints_match_storage: event_rows == checkpoint_rows,
        completed_full_passes,
        operational_mirrored_table_count,
        operational_rows,
        operational_expected_rows_at_start,
        operational_snapshot_counts_cover_source,
        operational_snapshot_mismatches,
        total_mirrored_rows: historian_catalog_rows
            .saturating_add(event_rows)
            .saturating_add(operational_rows),
        completed_operational_passes,
        reporting_rows,
        reporting_checkpoint_rows,
        reporting_checkpoints_match_storage: reporting_rows == reporting_checkpoint_rows,
        last_run_status: last_run.as_ref().map(|run| run.0.clone()),
        last_run_started_at: last_run.as_ref().map(|run| run.1.clone()),
        last_run_finished_at: last_run.and_then(|run| run.2),
        tables,
    })
}

pub fn inspect_configured_sql_mirror(settings: &SqlMirrorSettings) -> Result<SqlMirrorStatus> {
    settings.validate()?;
    let marker = Path::new(&settings.volume_marker);
    let metadata = fs::symlink_metadata(marker)
        .with_context(|| format!("external-volume marker is missing: {}", marker.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("external-volume marker must be a regular file, not a symlink");
    }
    let contents = fs::read_to_string(marker)
        .with_context(|| format!("read external-volume marker {}", marker.display()))?;
    if contents != SQL_MIRROR_VOLUME_MARKER_CONTENT {
        bail!("external-volume marker has unexpected contents");
    }
    inspect_sql_mirror(Path::new(&settings.target_database))
}

fn schema_rows_excluding(
    connection: &Connection,
    schema: &str,
    excluded_tables: &[&str],
) -> Result<u64> {
    let mut statement = connection.prepare(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = ? AND table_type = 'BASE TABLE' ORDER BY table_name",
    )?;
    let names = statement
        .query_map([schema], |row| row.get::<_, String>(0))?
        .collect::<duckdb::Result<Vec<_>>>()?;
    let mut total = 0u64;
    for name in names {
        if !excluded_tables.contains(&name.as_str()) {
            total = total.saturating_add(stored_table_rows(connection, schema, &name)?);
        }
    }
    Ok(total)
}

fn audit_operational_snapshots(
    connection: &Connection,
) -> Result<(u64, u64, u64, Vec<SqlMirrorSnapshotMismatch>)> {
    let mut statement = connection.prepare(
        "SELECT source_database, target_schema FROM metasys_migration.source_database_mappings ORDER BY source_database",
    )?;
    let mappings = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<duckdb::Result<Vec<_>>>()?;
    let mut expected_table_count = 0u64;
    let mut expected_rows = 0u64;
    let mut stored_rows = 0u64;
    let mut mismatches = Vec::new();

    for (source_database, target_schema) in mappings {
        let encoded = connection
            .query_row(
                "SELECT schema_json FROM metasys_migration.database_schema_snapshots WHERE source_database = ? ORDER BY captured_at DESC LIMIT 1",
                [&source_database],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .with_context(|| {
                format!("missing schema snapshot for SQL database {source_database}")
            })?;
        let snapshot: SourceDatabaseSnapshot = serde_json::from_str(&encoded)
            .with_context(|| format!("decode schema snapshot for {source_database}"))?;
        for table in snapshot.tables {
            expected_table_count = expected_table_count.saturating_add(1);
            let expected = nonnegative_u64(table.row_count)?;
            expected_rows = expected_rows.saturating_add(expected);
            let stored = if table_exists(connection, &target_schema, &table.table_name)? {
                Some(stored_table_rows(
                    connection,
                    &target_schema,
                    &table.table_name,
                )?)
            } else {
                None
            };
            if let Some(value) = stored {
                stored_rows = stored_rows.saturating_add(value);
            }
            if stored.is_none_or(|value| value < expected) {
                mismatches.push(SqlMirrorSnapshotMismatch {
                    source_database: source_database.clone(),
                    table_schema: table.table_schema,
                    table_name: table.table_name,
                    expected_rows_at_start: expected,
                    stored_rows: stored,
                });
            }
        }
    }

    Ok((expected_table_count, expected_rows, stored_rows, mismatches))
}

fn stored_table_rows(connection: &Connection, schema: &str, table: &str) -> Result<u64> {
    nonnegative_u64(connection.query_row(
        &format!(
            "SELECT COUNT(*)::BIGINT FROM {}.{}",
            quote_identifier(schema),
            quote_identifier(table)
        ),
        [],
        |row| row.get::<_, i64>(0),
    )?)
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn table_exists(connection: &Connection, schema: &str, table: &str) -> Result<bool> {
    let count = connection.query_row(
        "SELECT COUNT(*)::BIGINT FROM information_schema.tables WHERE table_schema = ? AND table_name = ? AND table_type = 'BASE TABLE'",
        [schema, table],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(count == 1)
}

fn nonnegative_u64(value: i64) -> Result<u64> {
    u64::try_from(value).context("DuckDB mirror count is negative")
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::{SqlMirrorRunRecord, SqlMirrorSettings, SqlMirrorSettingsUpdate};

    fn completed_run(status: &str) -> SqlMirrorRunRecord {
        let started_at = Utc::now() - Duration::minutes(30);
        SqlMirrorRunRecord {
            run_id: "run-1".to_owned(),
            started_at,
            finished_at: Some(started_at + Duration::minutes(4)),
            status: status.to_owned(),
            target_database: "/Volumes/Mirror/Metasys/history.duckdb".to_owned(),
            volume_marker: "/Volumes/Mirror/.metasys-storage-volume".to_owned(),
            duration_ms: Some(240_000),
            event_rows_copied: Some(25),
            event_rows_total: Some(5_000),
            source_event_rows: Some(5_000),
            total_mirrored_rows: Some(7_500),
            integrity_ok: Some(true),
            error_message: (status != "succeeded").then(|| "test failure".to_owned()),
        }
    }

    #[test]
    fn validates_mirror_configuration_bounds_and_volume_layout() {
        let valid = SqlMirrorSettingsUpdate {
            enabled: true,
            target_database: " /Volumes/Mirror/Metasys/history.duckdb ".to_owned(),
            volume_marker: " /Volumes/Mirror/.metasys-storage-volume ".to_owned(),
            interval_hours: 1,
            batch_size: 250_000,
        };
        assert!(valid.validated_settings().is_ok());

        for invalid in [
            SqlMirrorSettingsUpdate {
                interval_hours: 0,
                ..valid.clone()
            },
            SqlMirrorSettingsUpdate {
                target_database: "/tmp/history.duckdb".to_owned(),
                ..valid.clone()
            },
            SqlMirrorSettingsUpdate {
                target_database: "/Volumes/Mirror/../other/history.duckdb".to_owned(),
                ..valid.clone()
            },
            SqlMirrorSettingsUpdate {
                target_database: "/Volumes/Mirror/Metasys/history.sqlite".to_owned(),
                ..valid.clone()
            },
        ] {
            assert!(invalid.validated_settings().is_err());
        }
    }

    #[test]
    fn cadence_and_health_use_local_attempt_history() {
        let settings = SqlMirrorSettings {
            target_database: "/Volumes/Mirror/Metasys/history.duckdb".to_owned(),
            volume_marker: "/Volumes/Mirror/.metasys-storage-volume".to_owned(),
            interval_hours: 1,
            ..Default::default()
        };
        let success = completed_run("succeeded");
        assert!(!settings.is_due(success.started_at + Duration::minutes(59), Some(&success)));
        assert!(settings.is_due(success.started_at + Duration::hours(1), Some(&success)));

        let failed = completed_run("failed");
        let view = settings.view(vec![failed], Utc::now(), true);
        assert_eq!(view.health.state, "error");
        assert_eq!(view.health.consecutive_failures, 1);
        assert_eq!(view.health.last_error.as_deref(), Some("test failure"));
    }
}
