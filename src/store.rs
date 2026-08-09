use std::{path::Path, sync::Mutex};

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{
    config::MetasysConnectionSettings, email_reports::EmailReportSettings, models::AlarmRecord,
    sql_trends::SqlTrendSettings,
};

pub struct ReportDeliveryStatus {
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
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
                    error_message TEXT
                );

                CREATE TABLE IF NOT EXISTS app_settings (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );

                CREATE TABLE IF NOT EXISTS report_delivery_log (
                    attempted_at TEXT PRIMARY KEY,
                    succeeded INTEGER NOT NULL,
                    recipient_count INTEGER NOT NULL,
                    error_message TEXT
                );
                "#,
            )
            .context("initialize SQLite schema")?;

        ensure_occurrence_column(&connection)?;
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
                    id, object_id, equipment, point, message, alarm_type, category,
                    priority, occurred_at, active, acknowledged, occurrence_count,
                    source, last_seen_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                ON CONFLICT(id) DO UPDATE SET
                    object_id = excluded.object_id,
                    equipment = excluded.equipment,
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
        error_message: Option<&str>,
    ) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO poll_log (
                attempted_at, succeeded, active_alarm_count, override_count, error_message
            ) VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            params![
                Utc::now().to_rfc3339(),
                succeeded,
                active_alarm_count as i64,
                override_count as i64,
                error_message,
            ],
        )?;
        Ok(())
    }

    pub fn alarms_since(&self, since: DateTime<Utc>) -> Result<Vec<AlarmRecord>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            r#"
            SELECT id, object_id, equipment, point, message, alarm_type, category,
                   priority, occurred_at, active, acknowledged, occurrence_count, source
            FROM alarms
            WHERE occurred_at >= ?1
            ORDER BY occurred_at DESC
            "#,
        )?;
        let rows = statement.query_map([since.to_rfc3339()], |row| {
            let occurred_at: String = row.get(8)?;
            Ok(AlarmRecord {
                id: row.get(0)?,
                object_id: row.get(1)?,
                equipment: row.get(2)?,
                point: row.get(3)?,
                message: row.get(4)?,
                alarm_type: row.get(5)?,
                category: row.get(6)?,
                priority: row.get::<_, i64>(7)?.clamp(0, 255) as u16,
                occurred_at: DateTime::parse_from_rfc3339(&occurred_at)
                    .map(|value| value.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                active: row.get(9)?,
                acknowledged: row.get(10)?,
                occurrence_count: row.get::<_, i64>(11)?.max(1) as u64,
                source: row.get(12)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("read alarm history")
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
        value
            .map(|value| serde_json::from_str(&value).context("decode SQL trend settings"))
            .transpose()
            .map(Option::unwrap_or_default)
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

fn parse_stored_datetime(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
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
}
