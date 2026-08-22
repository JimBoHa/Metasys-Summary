use std::{path::Path, sync::Mutex};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    config::MetasysConnectionSettings,
    email_reports::EmailReportSettings,
    inventory::{EquipmentGroup, EquipmentInventory, EquipmentItem, EquipmentPoint},
    models::AlarmRecord,
    sql_mirror::{SqlMirrorRunRecord, SqlMirrorSettings},
    sql_trends::SqlTrendSettings,
};

pub struct ReportDeliveryStatus {
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PollRecord {
    pub attempted_at: DateTime<Utc>,
    pub succeeded: bool,
    pub active_alarm_count: usize,
    pub override_count: usize,
    pub duration_ms: Option<u64>,
    pub error_message: Option<String>,
}

pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)
            .with_context(|| format!("open SQLite database {}", path.display()))?;
        connection
            .execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                PRAGMA foreign_keys = ON;

                CREATE TABLE IF NOT EXISTS alarms (
                    id TEXT PRIMARY KEY,
                    object_id TEXT NOT NULL,
                    equipment TEXT NOT NULL,
                    equipment_origin TEXT NOT NULL DEFAULT 'unknown',
                    point TEXT NOT NULL,
                    message TEXT NOT NULL,
                    alarm_type TEXT NOT NULL,
                    category TEXT NOT NULL,
                    priority INTEGER NOT NULL,
                    occurred_at TEXT NOT NULL,
                    active INTEGER NOT NULL,
                    acknowledged INTEGER NOT NULL,
                    occurrence_count INTEGER NOT NULL DEFAULT 1,
                    source TEXT NOT NULL,
                    last_seen_at TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_alarms_occurred_at
                    ON alarms(occurred_at);
                CREATE INDEX IF NOT EXISTS idx_alarms_equipment
                    ON alarms(equipment);

                CREATE TABLE IF NOT EXISTS poll_log (
                    attempted_at TEXT PRIMARY KEY,
                    succeeded INTEGER NOT NULL,
                    active_alarm_count INTEGER NOT NULL,
                    override_count INTEGER NOT NULL,
                    duration_ms INTEGER,
                    error_message TEXT
                );

                CREATE TABLE IF NOT EXISTS app_settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS sql_mirror_run_log (
                    run_id TEXT PRIMARY KEY,
                    started_at TEXT NOT NULL,
                    finished_at TEXT,
                    status TEXT NOT NULL,
                    target_database TEXT NOT NULL,
                    volume_marker TEXT NOT NULL,
                    duration_ms INTEGER,
                    event_rows_copied INTEGER,
                    event_rows_total INTEGER,
                    source_event_rows INTEGER,
                    total_mirrored_rows INTEGER,
                    integrity_ok INTEGER,
                    error_message TEXT
                );

                CREATE INDEX IF NOT EXISTS idx_sql_mirror_run_log_started_at
                    ON sql_mirror_run_log(started_at DESC);

                CREATE TABLE IF NOT EXISTS report_delivery_log (
                    attempted_at TEXT PRIMARY KEY,
                    succeeded INTEGER NOT NULL,
                    recipient_count INTEGER NOT NULL,
                    error_message TEXT
                );

                CREATE TABLE IF NOT EXISTS equipment_inventory_meta (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    schema_version INTEGER NOT NULL,
                    root_name TEXT NOT NULL,
                    captured_at TEXT NOT NULL,
                    source_summary TEXT NOT NULL,
                    notes_json TEXT NOT NULL,
                    imported_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS equipment_inventory_groups (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL UNIQUE,
                    description TEXT NOT NULL,
                    sort_order INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS equipment_inventory_equipment (
                    id INTEGER PRIMARY KEY,
                    group_id INTEGER NOT NULL REFERENCES equipment_inventory_groups(id) ON DELETE CASCADE,
                    name TEXT NOT NULL,
                    equipment_type TEXT NOT NULL,
                    variant TEXT NOT NULL,
                    protocol TEXT NOT NULL,
                    network_name TEXT NOT NULL,
                    mac_address INTEGER,
                    device_instance INTEGER,
                    object_reference TEXT NOT NULL,
                    discovery_status TEXT NOT NULL,
                    source TEXT NOT NULL,
                    sort_order INTEGER NOT NULL,
                    UNIQUE(group_id, name)
                );

                CREATE TABLE IF NOT EXISTS equipment_inventory_points (
                    id INTEGER PRIMARY KEY,
                    equipment_id INTEGER NOT NULL REFERENCES equipment_inventory_equipment(id) ON DELETE CASCADE,
                    name TEXT NOT NULL,
                    reference TEXT NOT NULL,
                    category TEXT NOT NULL,
                    unit TEXT,
                    historian_point_slice_id INTEGER,
                    source TEXT NOT NULL,
                    sort_order INTEGER NOT NULL,
                    UNIQUE(equipment_id, name)
                );

                CREATE INDEX IF NOT EXISTS idx_equipment_inventory_equipment_group
                    ON equipment_inventory_equipment(group_id, sort_order);
                CREATE INDEX IF NOT EXISTS idx_equipment_inventory_points_equipment
                    ON equipment_inventory_points(equipment_id, sort_order);
                "#,
            )
            .context("initialize SQLite schema")?;

        ensure_occurrence_column(&connection)?;
        ensure_equipment_origin_column(&connection)?;
        ensure_poll_duration_column(&connection)?;
        crate::portal::store::initialize_schema(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn upsert_alarms(&self, alarms: &[AlarmRecord]) -> Result<()> {
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .context("begin alarm transaction")?;
        let now = Utc::now().to_rfc3339();
        {
            let mut statement = transaction.prepare_cached(
                r#"
                INSERT INTO alarms (
                    id, object_id, equipment, equipment_origin, point, message, alarm_type, category,
                    priority, occurred_at, active, acknowledged, occurrence_count,
                    source, last_seen_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                ON CONFLICT(id) DO UPDATE SET
                    object_id = excluded.object_id,
                    equipment = excluded.equipment,
                    equipment_origin = excluded.equipment_origin,
                    point = excluded.point,
                    message = excluded.message,
                    alarm_type = excluded.alarm_type,
                    category = excluded.category,
                    priority = excluded.priority,
                    occurred_at = excluded.occurred_at,
                    active = excluded.active,
                    acknowledged = excluded.acknowledged,
                    occurrence_count = MAX(alarms.occurrence_count, excluded.occurrence_count),
                    source = excluded.source,
                    last_seen_at = excluded.last_seen_at
                "#,
            )?;

            for alarm in alarms {
                statement.execute(params![
                    alarm.id,
                    alarm.object_id,
                    alarm.equipment,
                    alarm.equipment_origin,
                    alarm.point,
                    alarm.message,
                    alarm.alarm_type,
                    alarm.category,
                    i64::from(alarm.priority),
                    alarm.occurred_at.to_rfc3339(),
                    alarm.active,
                    alarm.acknowledged,
                    alarm.occurrence_count as i64,
                    alarm.source,
                    now,
                ])?;
            }
        }
        transaction.commit().context("commit alarm transaction")
    }

    pub fn record_poll(
        &self,
        succeeded: bool,
        active_alarm_count: usize,
        override_count: usize,
        duration_ms: u64,
        error_message: Option<&str>,
    ) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO poll_log (
                attempted_at, succeeded, active_alarm_count, override_count, duration_ms, error_message
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                Utc::now().to_rfc3339(),
                succeeded,
                active_alarm_count as i64,
                override_count as i64,
                duration_ms.min(i64::MAX as u64) as i64,
                error_message.map(|message| message.chars().take(1_000).collect::<String>()),
            ],
        )?;
        Ok(())
    }

    pub fn alarms_since(&self, since: DateTime<Utc>) -> Result<Vec<AlarmRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, object_id, equipment, equipment_origin, point, message, alarm_type, category,
                   priority, occurred_at, active, acknowledged, occurrence_count, source, last_seen_at
            FROM alarms
            WHERE occurred_at >= ?1
            ORDER BY occurred_at DESC
            "#,
        )?;
        let rows = statement.query_map([since.to_rfc3339()], |row| {
            let occurred_at: String = row.get(9)?;
            let last_seen_at: String = row.get(14)?;
            Ok(AlarmRecord {
                id: row.get(0)?,
                object_id: row.get(1)?,
                equipment: row.get(2)?,
                equipment_origin: row.get(3)?,
                point: row.get(4)?,
                message: row.get(5)?,
                alarm_type: row.get(6)?,
                category: row.get(7)?,
                priority: row.get::<_, i64>(8)?.clamp(0, 255) as u16,
                occurred_at: DateTime::parse_from_rfc3339(&occurred_at)
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                active: row.get(10)?,
                acknowledged: row.get(11)?,
                occurrence_count: row.get::<_, i64>(12)?.max(1) as u64,
                source: row.get(13)?,
                last_seen_at: parse_stored_datetime(&last_seen_at),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("read alarm history")
    }

    pub fn polls_since(&self, since: DateTime<Utc>) -> Result<Vec<PollRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            r#"
            SELECT attempted_at, succeeded, active_alarm_count, override_count,
                   duration_ms, error_message
            FROM poll_log
            WHERE attempted_at >= ?1
            ORDER BY attempted_at DESC
            "#,
        )?;
        let rows = statement.query_map([since.to_rfc3339()], |row| {
            let attempted_at: String = row.get(0)?;
            Ok(PollRecord {
                attempted_at: parse_stored_datetime(&attempted_at).unwrap_or_else(Utc::now),
                succeeded: row.get(1)?,
                active_alarm_count: row.get::<_, i64>(2)?.max(0) as usize,
                override_count: row.get::<_, i64>(3)?.max(0) as usize,
                duration_ms: row
                    .get::<_, Option<i64>>(4)?
                    .filter(|duration| *duration > 0)
                    .map(|duration| duration as u64),
                error_message: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("read poll history")
    }

    pub fn first_alarm_at(&self) -> Result<Option<DateTime<Utc>>> {
        let connection = self.lock()?;
        let value: Option<String> = connection
            .query_row("SELECT MIN(occurred_at) FROM alarms", [], |row| row.get(0))
            .optional()?
            .flatten();
        Ok(value.and_then(|timestamp| {
            DateTime::parse_from_rfc3339(&timestamp)
                .ok()
                .map(|value| value.with_timezone(&Utc))
        }))
    }

    pub fn prune(&self, retention_days: i64) -> Result<()> {
        let connection = self.lock()?;
        let cutoff = (Utc::now() - Duration::days(retention_days.max(31))).to_rfc3339();
        connection.execute("DELETE FROM alarms WHERE occurred_at < ?1", [&cutoff])?;
        connection.execute("DELETE FROM poll_log WHERE attempted_at < ?1", [&cutoff])?;
        connection.execute(
            "DELETE FROM report_delivery_log WHERE attempted_at < ?1",
            [&cutoff],
        )?;
        Ok(())
    }

    pub fn email_report_settings(&self) -> Result<EmailReportSettings> {
        let connection = self.lock()?;
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'email_reports'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| serde_json::from_str(&value).context("decode email report settings"))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    pub fn save_email_report_settings(&self, settings: &EmailReportSettings) -> Result<()> {
        let connection = self.lock()?;
        let value = serde_json::to_string(settings).context("encode email report settings")?;
        connection.execute(
            r#"
            INSERT INTO app_settings (key, value, updated_at)
            VALUES ('email_reports', ?1, ?2)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at
            "#,
            params![value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn sql_trend_settings(&self) -> Result<SqlTrendSettings> {
        let connection = self.lock()?;
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'sql_trends'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let settings: SqlTrendSettings = value
            .map(|value| serde_json::from_str(&value).context("decode SQL trend settings"))
            .transpose()
            .map(Option::unwrap_or_default)?;
        Ok(settings.upgrade_legacy_defaults())
    }

    pub fn save_sql_trend_settings(&self, settings: &SqlTrendSettings) -> Result<()> {
        let connection = self.lock()?;
        let value = serde_json::to_string(settings).context("encode SQL trend settings")?;
        connection.execute(
            r#"
            INSERT INTO app_settings (key, value, updated_at)
            VALUES ('sql_trends', ?1, ?2)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at
            "#,
            params![value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn sql_mirror_settings(&self) -> Result<SqlMirrorSettings> {
        let connection = self.lock()?;
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'sql_mirror'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| serde_json::from_str(&value).context("decode SQL mirror settings"))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    pub fn save_sql_mirror_settings(&self, settings: &SqlMirrorSettings) -> Result<()> {
        settings.validate()?;
        let connection = self.lock()?;
        let value = serde_json::to_string(settings).context("encode SQL mirror settings")?;
        connection.execute(
            r#"
            INSERT INTO app_settings (key, value, updated_at)
            VALUES ('sql_mirror', ?1, ?2)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at
            "#,
            params![value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn begin_sql_mirror_run(
        &self,
        run_id: &str,
        settings: &SqlMirrorSettings,
        started_at: DateTime<Utc>,
    ) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO sql_mirror_run_log (
                run_id, started_at, status, target_database, volume_marker
            ) VALUES (?1, ?2, 'running', ?3, ?4)
            "#,
            params![
                run_id,
                started_at.to_rfc3339(),
                settings.target_database,
                settings.volume_marker
            ],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn finish_sql_mirror_run(
        &self,
        run_id: &str,
        status: &str,
        finished_at: DateTime<Utc>,
        duration_ms: u64,
        event_rows_copied: Option<u64>,
        event_rows_total: Option<u64>,
        source_event_rows: Option<u64>,
        total_mirrored_rows: Option<u64>,
        integrity_ok: Option<bool>,
        error_message: Option<&str>,
    ) -> Result<()> {
        if !matches!(status, "succeeded" | "failed") {
            return Err(anyhow!("invalid SQL mirror completion status '{status}'"));
        }
        let connection = self.lock()?;
        let changed = connection.execute(
            r#"
            UPDATE sql_mirror_run_log
            SET finished_at = ?2,
                status = ?3,
                duration_ms = ?4,
                event_rows_copied = ?5,
                event_rows_total = ?6,
                source_event_rows = ?7,
                total_mirrored_rows = ?8,
                integrity_ok = ?9,
                error_message = ?10
            WHERE run_id = ?1 AND status = 'running'
            "#,
            params![
                run_id,
                finished_at.to_rfc3339(),
                status,
                sqlite_u64(duration_ms),
                event_rows_copied.map(sqlite_u64),
                event_rows_total.map(sqlite_u64),
                source_event_rows.map(sqlite_u64),
                total_mirrored_rows.map(sqlite_u64),
                integrity_ok,
                error_message.map(|message| message.chars().take(4_000).collect::<String>()),
            ],
        )?;
        if changed != 1 {
            return Err(anyhow!(
                "SQL mirror run {run_id} was missing or no longer running"
            ));
        }
        connection.execute(
            r#"
            DELETE FROM sql_mirror_run_log
            WHERE run_id NOT IN (
                SELECT run_id FROM sql_mirror_run_log
                ORDER BY started_at DESC LIMIT 200
            )
            "#,
            [],
        )?;
        Ok(())
    }

    pub fn mark_interrupted_sql_mirror_runs(&self, finished_at: DateTime<Utc>) -> Result<usize> {
        let connection = self.lock()?;
        connection
            .execute(
                r#"
                UPDATE sql_mirror_run_log
                SET finished_at = ?1,
                    status = 'interrupted',
                    duration_ms = CAST(MAX(0, (julianday(?1) - julianday(started_at)) * 86400000) AS INTEGER),
                    error_message = 'Previous scheduler process ended before recording completion'
                WHERE status = 'running'
                "#,
                [finished_at.to_rfc3339()],
            )
            .context("mark interrupted SQL mirror runs")
    }

    pub fn recent_sql_mirror_runs(&self, limit: usize) -> Result<Vec<SqlMirrorRunRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            r#"
            SELECT run_id, started_at, finished_at, status, target_database, volume_marker,
                   duration_ms, event_rows_copied, event_rows_total, source_event_rows,
                   total_mirrored_rows, integrity_ok, error_message
            FROM sql_mirror_run_log
            ORDER BY started_at DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map([limit.clamp(1, 200) as i64], |row| {
            let started_at: String = row.get(1)?;
            let finished_at: Option<String> = row.get(2)?;
            Ok(SqlMirrorRunRecord {
                run_id: row.get(0)?,
                started_at: parse_stored_datetime(&started_at).unwrap_or_else(Utc::now),
                finished_at: finished_at.as_deref().and_then(parse_stored_datetime),
                status: row.get(3)?,
                target_database: row.get(4)?,
                volume_marker: row.get(5)?,
                duration_ms: row.get::<_, Option<i64>>(6)?.map(nonnegative_sqlite_u64),
                event_rows_copied: row.get::<_, Option<i64>>(7)?.map(nonnegative_sqlite_u64),
                event_rows_total: row.get::<_, Option<i64>>(8)?.map(nonnegative_sqlite_u64),
                source_event_rows: row.get::<_, Option<i64>>(9)?.map(nonnegative_sqlite_u64),
                total_mirrored_rows: row.get::<_, Option<i64>>(10)?.map(nonnegative_sqlite_u64),
                integrity_ok: row.get(11)?,
                error_message: row.get(12)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("read SQL mirror run history")
    }

    pub fn metasys_connection_settings(&self) -> Result<Option<MetasysConnectionSettings>> {
        let connection = self.lock()?;
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'metasys_connection'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|value| serde_json::from_str(&value).context("decode Metasys connection settings"))
            .transpose()
    }

    pub fn save_metasys_connection_settings(
        &self,
        settings: &MetasysConnectionSettings,
    ) -> Result<()> {
        let connection = self.lock()?;
        let value =
            serde_json::to_string(settings).context("encode Metasys connection settings")?;
        connection.execute(
            r#"
            INSERT INTO app_settings (key, value, updated_at)
            VALUES ('metasys_connection', ?1, ?2)
            ON CONFLICT(key) DO UPDATE SET
                value = excluded.value,
                updated_at = excluded.updated_at
            "#,
            params![value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn replace_equipment_inventory(&self, inventory: &EquipmentInventory) -> Result<()> {
        inventory.validate()?;
        let mut connection = self.lock()?;
        let transaction = connection
            .transaction()
            .context("begin equipment inventory transaction")?;
        transaction.execute("DELETE FROM equipment_inventory_groups", [])?;
        transaction.execute("DELETE FROM equipment_inventory_meta", [])?;
        transaction.execute(
            r#"
            INSERT INTO equipment_inventory_meta (
                id, schema_version, root_name, captured_at, source_summary, notes_json, imported_at
            ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6)
            "#,
            params![
                i64::from(inventory.schema_version),
                inventory.root_name,
                inventory.captured_at.to_rfc3339(),
                inventory.source_summary,
                serde_json::to_string(&inventory.notes).context("encode inventory notes")?,
                Utc::now().to_rfc3339(),
            ],
        )?;

        for (group_index, group) in inventory.groups.iter().enumerate() {
            transaction.execute(
                r#"
                INSERT INTO equipment_inventory_groups (name, description, sort_order)
                VALUES (?1, ?2, ?3)
                "#,
                params![group.name, group.description, group_index as i64],
            )?;
            let group_id = transaction.last_insert_rowid();
            for (equipment_index, equipment) in group.equipment.iter().enumerate() {
                transaction.execute(
                    r#"
                    INSERT INTO equipment_inventory_equipment (
                        group_id, name, equipment_type, variant, protocol, network_name,
                        mac_address, device_instance, object_reference, discovery_status,
                        source, sort_order
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                    "#,
                    params![
                        group_id,
                        equipment.name,
                        equipment.equipment_type,
                        equipment.variant,
                        equipment.protocol,
                        equipment.network_name,
                        equipment.mac_address.map(i64::from),
                        equipment.device_instance.map(i64::from),
                        equipment.object_reference,
                        equipment.discovery_status,
                        equipment.source,
                        equipment_index as i64,
                    ],
                )?;
                let equipment_id = transaction.last_insert_rowid();
                for (point_index, point) in equipment.points.iter().enumerate() {
                    transaction.execute(
                        r#"
                        INSERT INTO equipment_inventory_points (
                            equipment_id, name, reference, category, unit,
                            historian_point_slice_id, source, sort_order
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                        "#,
                        params![
                            equipment_id,
                            point.name,
                            point.reference,
                            point.category,
                            point.unit,
                            point.historian_point_slice_id,
                            point.source,
                            point_index as i64,
                        ],
                    )?;
                }
            }
        }
        transaction
            .commit()
            .context("commit equipment inventory transaction")
    }

    pub fn equipment_inventory(&self) -> Result<Option<EquipmentInventory>> {
        let connection = self.lock()?;
        let metadata = connection
            .query_row(
                r#"
                SELECT schema_version, root_name, captured_at, source_summary, notes_json
                FROM equipment_inventory_meta
                WHERE id = 1
                "#,
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((schema_version, root_name, captured_at, source_summary, notes_json)) = metadata
        else {
            return Ok(None);
        };

        let mut group_statement = connection.prepare(
            r#"
            SELECT id, name, description
            FROM equipment_inventory_groups
            ORDER BY sort_order, id
            "#,
        )?;
        let group_rows = group_statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut groups = Vec::with_capacity(group_rows.len());
        for (group_id, name, description) in group_rows {
            let mut equipment_statement = connection.prepare(
                r#"
                SELECT id, name, equipment_type, variant, protocol, network_name,
                       mac_address, device_instance, object_reference, discovery_status, source
                FROM equipment_inventory_equipment
                WHERE group_id = ?1
                ORDER BY sort_order, id
                "#,
            )?;
            let equipment_rows = equipment_statement
                .query_map([group_id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, Option<i64>>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut equipment = Vec::with_capacity(equipment_rows.len());
            for (
                equipment_id,
                equipment_name,
                equipment_type,
                variant,
                protocol,
                network_name,
                mac_address,
                device_instance,
                object_reference,
                discovery_status,
                source,
            ) in equipment_rows
            {
                let mut point_statement = connection.prepare(
                    r#"
                    SELECT name, reference, category, unit, historian_point_slice_id, source
                    FROM equipment_inventory_points
                    WHERE equipment_id = ?1
                    ORDER BY sort_order, id
                    "#,
                )?;
                let points = point_statement
                    .query_map([equipment_id], |row| {
                        Ok(EquipmentPoint {
                            name: row.get(0)?,
                            reference: row.get(1)?,
                            category: row.get(2)?,
                            unit: row.get(3)?,
                            historian_point_slice_id: row.get(4)?,
                            source: row.get(5)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                equipment.push(EquipmentItem {
                    name: equipment_name,
                    equipment_type,
                    variant,
                    protocol,
                    network_name,
                    mac_address: mac_address.map(|value| value.clamp(0, 127) as u16),
                    device_instance: device_instance.map(|value| value.clamp(0, 4_194_303) as u32),
                    object_reference,
                    discovery_status,
                    source,
                    points,
                });
            }
            groups.push(EquipmentGroup {
                name,
                description,
                equipment,
            });
        }
        Ok(Some(EquipmentInventory {
            schema_version: schema_version.max(0) as u32,
            root_name,
            captured_at: parse_stored_datetime(&captured_at).unwrap_or_else(Utc::now),
            source_summary,
            notes: serde_json::from_str(&notes_json).context("decode inventory notes")?,
            groups,
        }))
    }

    pub fn record_report_delivery(
        &self,
        succeeded: bool,
        recipient_count: usize,
        error_message: Option<&str>,
    ) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO report_delivery_log (
                attempted_at, succeeded, recipient_count, error_message
            ) VALUES (?1, ?2, ?3, ?4)
            "#,
            params![
                Utc::now().to_rfc3339(),
                succeeded,
                recipient_count as i64,
                error_message.map(|message| message.chars().take(1_000).collect::<String>()),
            ],
        )?;
        Ok(())
    }

    pub fn report_delivery_status(&self) -> Result<ReportDeliveryStatus> {
        let connection = self.lock()?;
        let latest: Option<(String, bool, Option<String>)> = connection
            .query_row(
                r#"
                SELECT attempted_at, succeeded, error_message
                FROM report_delivery_log
                ORDER BY attempted_at DESC
                LIMIT 1
                "#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let last_success: Option<String> = connection
            .query_row(
                "SELECT MAX(attempted_at) FROM report_delivery_log WHERE succeeded = 1",
                [],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let (last_attempt_at, last_error) = latest
            .map(|(timestamp, succeeded, error)| {
                (
                    parse_stored_datetime(&timestamp),
                    if succeeded { None } else { error },
                )
            })
            .unwrap_or((None, None));
        Ok(ReportDeliveryStatus {
            last_attempt_at,
            last_success_at: last_success.and_then(|value| parse_stored_datetime(&value)),
            last_error,
        })
    }
    pub(crate) fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow!("SQLite connection lock poisoned"))
    }
}

fn ensure_occurrence_column(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(alarms)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == "occurrence_count") {
        connection.execute(
            "ALTER TABLE alarms ADD COLUMN occurrence_count INTEGER NOT NULL DEFAULT 1",
            [],
        )?;
    }
    Ok(())
}

fn ensure_equipment_origin_column(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(alarms)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == "equipment_origin") {
        connection.execute(
            "ALTER TABLE alarms ADD COLUMN equipment_origin TEXT NOT NULL DEFAULT 'unknown'",
            [],
        )?;
        connection.execute(
            "UPDATE alarms SET equipment_origin = 'server' WHERE equipment <> 'Unmapped equipment'",
            [],
        )?;
    }
    Ok(())
}

fn ensure_poll_duration_column(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(poll_log)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns.iter().any(|column| column == "duration_ms") {
        connection.execute("ALTER TABLE poll_log ADD COLUMN duration_ms INTEGER", [])?;
    }
    Ok(())
}

fn parse_stored_datetime(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn sqlite_u64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn nonnegative_sqlite_u64(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::Store;
    use crate::models::AlarmRecord;

    #[test]
    fn upserts_without_duplicating_alarm() {
        let directory = tempdir().unwrap();
        let store = Store::open(&directory.path().join("test.sqlite3")).unwrap();
        let mut alarm = AlarmRecord {
            id: "alarm-1".to_owned(),
            object_id: "point-1".to_owned(),
            equipment: "AHU-1".to_owned(),
            equipment_origin: "server".to_owned(),
            point: "SAT".to_owned(),
            message: "High temperature".to_owned(),
            alarm_type: "avHighLimit".to_owned(),
            category: "hvacCategory".to_owned(),
            priority: 40,
            occurred_at: Utc::now(),
            active: true,
            acknowledged: false,
            occurrence_count: 1,
            source: "test".to_owned(),
            last_seen_at: None,
        };
        store.upsert_alarms(&[alarm.clone()]).unwrap();
        alarm.occurrence_count = 4;
        store.upsert_alarms(&[alarm]).unwrap();

        let alarms = store
            .alarms_since(Utc::now() - chrono::Duration::days(1))
            .unwrap();
        assert_eq!(alarms.len(), 1);
        assert_eq!(alarms[0].occurrence_count, 4);
    }

    #[test]
    fn migrates_existing_alarm_and_poll_tables_for_diagnostics() {
        let directory = tempdir().unwrap();
        let database_path = directory.path().join("existing.sqlite3");
        let connection = Connection::open(&database_path).unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE alarms (
                    id TEXT PRIMARY KEY, object_id TEXT NOT NULL, equipment TEXT NOT NULL,
                    point TEXT NOT NULL, message TEXT NOT NULL, alarm_type TEXT NOT NULL,
                    category TEXT NOT NULL, priority INTEGER NOT NULL, occurred_at TEXT NOT NULL,
                    active INTEGER NOT NULL, acknowledged INTEGER NOT NULL,
                    occurrence_count INTEGER NOT NULL DEFAULT 1, source TEXT NOT NULL,
                    last_seen_at TEXT NOT NULL
                );
                CREATE TABLE poll_log (
                    attempted_at TEXT PRIMARY KEY, succeeded INTEGER NOT NULL,
                    active_alarm_count INTEGER NOT NULL, override_count INTEGER NOT NULL,
                    error_message TEXT
                );
                "#,
            )
            .unwrap();
        let now = Utc::now().to_rfc3339();
        connection
            .execute(
                "INSERT INTO alarms VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 40, ?8, 1, 0, 1, 'legacy-ui', ?8)",
                rusqlite::params![
                    "alarm-1",
                    "SERVER:NAE/AHU-1.SAT",
                    "AHU-1",
                    "SAT",
                    "High temperature",
                    "High Alarm",
                    "HVAC",
                    now,
                ],
            )
            .unwrap();
        connection
            .execute("INSERT INTO poll_log VALUES (?1, 1, 1, 0, NULL)", [&now])
            .unwrap();
        drop(connection);

        let store = Store::open(&database_path).unwrap();
        let alarms = store
            .alarms_since(Utc::now() - chrono::Duration::days(1))
            .unwrap();
        assert_eq!(alarms[0].equipment_origin, "server");
        let polls = store
            .polls_since(Utc::now() - chrono::Duration::days(1))
            .unwrap();
        assert_eq!(polls.len(), 1);
        assert_eq!(polls[0].duration_ms, None);
        store.record_poll(true, 2, 1, 375, None).unwrap();
        assert!(
            store
                .polls_since(Utc::now() - chrono::Duration::days(1))
                .unwrap()
                .iter()
                .any(|poll| poll.duration_ms == Some(375))
        );
    }

    #[test]
    fn stores_report_settings_and_delivery_status() {
        let directory = tempdir().unwrap();
        let store = Store::open(&directory.path().join("test.sqlite3")).unwrap();
        let settings = crate::email_reports::EmailReportSettings::default();
        store.save_email_report_settings(&settings).unwrap();
        assert!(!store.email_report_settings().unwrap().enabled);
        store
            .record_report_delivery(false, 2, Some("SMTP unavailable"))
            .unwrap();
        let status = store.report_delivery_status().unwrap();
        assert!(status.last_attempt_at.is_some());
        assert!(status.last_success_at.is_none());
        assert_eq!(status.last_error.as_deref(), Some("SMTP unavailable"));
    }

    #[test]
    fn stores_non_secret_sql_trend_settings() {
        let directory = tempdir().unwrap();
        let store = Store::open(&directory.path().join("test.sqlite3")).unwrap();
        let settings = crate::sql_trends::SqlTrendSettings {
            host: "sql.example.invalid".to_owned(),
            database: "MetasysTrends".to_owned(),
            username: "trend_reader".to_owned(),
            ..Default::default()
        };
        store.save_sql_trend_settings(&settings).unwrap();
        let loaded = store.sql_trend_settings().unwrap();
        assert_eq!(loaded.host, "sql.example.invalid");
        assert_eq!(loaded.database, "MetasysTrends");
    }

    #[test]
    fn stores_sql_mirror_settings_and_bounded_run_health() {
        let directory = tempdir().unwrap();
        let store = Store::open(&directory.path().join("test.sqlite3")).unwrap();
        let settings = crate::sql_mirror::SqlMirrorSettings {
            enabled: true,
            target_database: "/Volumes/Mirror/Metasys/history.duckdb".to_owned(),
            volume_marker: "/Volumes/Mirror/.metasys-storage-volume".to_owned(),
            interval_hours: 6,
            batch_size: 100_000,
        };
        store.save_sql_mirror_settings(&settings).unwrap();
        assert_eq!(store.sql_mirror_settings().unwrap(), settings);

        let started_at = Utc::now() - chrono::Duration::minutes(3);
        store
            .begin_sql_mirror_run("run-success", &settings, started_at)
            .unwrap();
        store
            .finish_sql_mirror_run(
                "run-success",
                "succeeded",
                Utc::now(),
                180_000,
                Some(42),
                Some(5_000),
                Some(5_000),
                Some(7_500),
                Some(true),
                None,
            )
            .unwrap();
        store
            .begin_sql_mirror_run("run-interrupted", &settings, Utc::now())
            .unwrap();
        assert_eq!(
            store.mark_interrupted_sql_mirror_runs(Utc::now()).unwrap(),
            1
        );

        let runs = store.recent_sql_mirror_runs(10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].status, "interrupted");
        assert!(runs[0].error_message.as_deref().unwrap().contains("ended"));
        assert_eq!(runs[1].event_rows_copied, Some(42));
        assert_eq!(runs[1].integrity_ok, Some(true));
    }

    #[test]
    fn stores_non_secret_metasys_connection_settings() {
        let directory = tempdir().unwrap();
        let store = Store::open(&directory.path().join("test.sqlite3")).unwrap();
        let settings = crate::config::MetasysConnectionSettings {
            server_url: "https://metasys.example.test".to_owned(),
            username: "browser-user".to_owned(),
            domain: "Metasys Local".to_owned(),
            connector: crate::config::ConnectorPreference::Legacy,
            api_version: "auto".to_owned(),
            accept_invalid_certificates: true,
        };
        store.save_metasys_connection_settings(&settings).unwrap();
        let loaded = store.metasys_connection_settings().unwrap().unwrap();
        assert_eq!(loaded, settings);
        let connection = store.lock().unwrap();
        let stored: String = connection
            .query_row(
                "SELECT value FROM app_settings WHERE key = 'metasys_connection'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!stored.to_ascii_lowercase().contains("password"));
    }

    #[test]
    fn replaces_and_reads_equipment_inventory() {
        let directory = tempdir().unwrap();
        let store = Store::open(&directory.path().join("test.sqlite3")).unwrap();
        let inventory = crate::inventory::EquipmentInventory {
            schema_version: 1,
            root_name: "B Mod".to_owned(),
            captured_at: Utc::now(),
            source_summary: "Passive MS/TP discovery".to_owned(),
            notes: vec!["NAE offline".to_owned()],
            groups: vec![crate::inventory::EquipmentGroup {
                name: "VAVs".to_owned(),
                description: "Terminal boxes".to_owned(),
                equipment: vec![crate::inventory::EquipmentItem {
                    name: "TB2-101".to_owned(),
                    equipment_type: "terminalBox".to_owned(),
                    variant: "fanPoweredHeating".to_owned(),
                    protocol: "BACnet MS/TP".to_owned(),
                    network_name: "B2-NAE / FC-1".to_owned(),
                    mac_address: Some(4),
                    device_instance: Some(1_049_854),
                    object_reference: "BMSServer:B2-NAE/FC-1.021004FE".to_owned(),
                    discovery_status: "Active on trunk".to_owned(),
                    source: "passive scan".to_owned(),
                    points: vec![crate::inventory::EquipmentPoint {
                        name: "ZN-T".to_owned(),
                        reference: "BMSServer:B2-NAE/FC-1.021004FE.ZN-T".to_owned(),
                        category: "temperature".to_owned(),
                        unit: Some("degF".to_owned()),
                        historian_point_slice_id: Some(7126),
                        source: "historian".to_owned(),
                    }],
                }],
            }],
        };
        store.replace_equipment_inventory(&inventory).unwrap();
        assert_eq!(store.equipment_inventory().unwrap(), Some(inventory));
    }
}
