use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use tokio::sync::{Mutex, RwLock};

use crate::{
    analytics,
    config::Config,
    email_reports::{
        EmailDeliveryResult, EmailReportSettings, EmailReportSettingsUpdate,
        EmailReportSettingsView, clear_smtp_password, send_report, set_smtp_password, test_smtp,
    },
    metasys::MetasysClient,
    models::{AlarmRecord, DashboardView, HealthView, OverrideRecord},
    store::Store,
};

#[derive(Default)]
struct LiveData {
    health: HealthView,
    active_alarms: Vec<AlarmRecord>,
    overrides: Vec<OverrideRecord>,
}

pub struct AppState {
    pub config: Arc<Config>,
    store: Arc<Store>,
    client: MetasysClient,
    live: RwLock<LiveData>,
    poll_lock: Mutex<()>,
    report_send_lock: Mutex<()>,
}

impl AppState {
    pub fn new(config: Arc<Config>, store: Arc<Store>) -> Result<Self> {
        let client = MetasysClient::new(config.clone())?;
        Ok(Self {
            config,
            store,
            client,
            live: RwLock::new(LiveData::default()),
            poll_lock: Mutex::new(()),
            report_send_lock: Mutex::new(()),
        })
    }

    pub async fn poll_once(&self) {
        let Ok(_poll_guard) = self.poll_lock.try_lock() else {
            return;
        };
        let attempted_at = Utc::now();
        {
            let mut live = self.live.write().await;
            live.health.last_attempt_at = Some(attempted_at);
            if live.health.state == "starting" {
                live.health.message = "Connecting to Metasys".to_owned();
            }
        }

        match self.client.fetch().await {
            Ok(data) => {
                if let Err(error) = self.store.upsert_alarms(&data.alarms) {
                    self.record_failure(error).await;
                    return;
                }
                if let Err(error) = self.store.record_poll(
                    true,
                    data.active_alarms.len(),
                    data.overrides.len(),
                    None,
                ) {
                    tracing::warn!(error = %error, "failed to record successful poll");
                }
                if let Err(error) = self.store.prune(self.config.history_days + 7) {
                    tracing::warn!(error = %error, "failed to prune dashboard history");
                }

                let is_demo = data.connector == "Demo data";
                let mut live = self.live.write().await;
                live.health = HealthView {
                    state: if is_demo { "demo" } else { "ok" }.to_owned(),
                    message: if is_demo {
                        "Showing generated demonstration data".to_owned()
                    } else {
                        "Metasys data current".to_owned()
                    },
                    connector: data.connector,
                    server_version: data.server_version,
                    last_success_at: Some(Utc::now()),
                    last_attempt_at: Some(attempted_at),
                    history_started_at: live.health.history_started_at,
                };
                live.active_alarms = data.active_alarms;
                live.overrides = data.overrides;
            }
            Err(error) => self.record_failure(error).await,
        }
    }

    async fn record_failure(&self, error: anyhow::Error) {
        let message = error.to_string();
        tracing::warn!(error = %message, "Metasys poll failed");
        let (active_count, override_count) = {
            let live = self.live.read().await;
            (live.active_alarms.len(), live.overrides.len())
        };
        if let Err(store_error) =
            self.store
                .record_poll(false, active_count, override_count, Some(&message))
        {
            tracing::warn!(error = %store_error, "failed to record unsuccessful poll");
        }
        let mut live = self.live.write().await;
        live.health.state = "error".to_owned();
        live.health.message = message;
        live.health.last_attempt_at = Some(Utc::now());
    }

    pub async fn dashboard(&self) -> Result<DashboardView> {
        let live = self.live.read().await;
        analytics::build_dashboard(
            &self.store,
            live.health.clone(),
            &live.active_alarms,
            &live.overrides,
            self.config.history_days,
        )
    }

    pub async fn health(&self) -> HealthView {
        self.live.read().await.health.clone()
    }

    pub fn email_report_settings(&self) -> Result<EmailReportSettingsView> {
        let settings = self.store.email_report_settings()?;
        let status = self.store.report_delivery_status()?;
        Ok(settings.view(
            status.last_attempt_at,
            status.last_success_at,
            status.last_error,
        ))
    }

    pub fn update_email_report_settings(
        &self,
        update: EmailReportSettingsUpdate,
    ) -> Result<EmailReportSettingsView> {
        let settings = update.validated_settings()?;
        if update.clear_password {
            clear_smtp_password()?;
        } else if let Some(password) = update.smtp_password.as_deref()
            && !password.is_empty()
        {
            set_smtp_password(password)?;
        }
        self.store.save_email_report_settings(&settings)?;
        self.email_report_settings()
    }

    pub async fn test_email_report_connection(&self) -> Result<()> {
        let settings = self.store.email_report_settings()?;
        test_smtp(&settings).await
    }

    pub async fn send_email_report_now(&self) -> Result<EmailDeliveryResult> {
        let _guard = self.report_send_lock.lock().await;
        let settings = self.store.email_report_settings()?;
        self.deliver_email_report(&settings).await
    }

    pub async fn send_scheduled_email_report(&self) {
        let Ok(settings) = self.store.email_report_settings() else {
            return;
        };
        let Ok(status) = self.store.report_delivery_status() else {
            return;
        };
        if !settings.is_due(
            chrono::Local::now(),
            status.last_attempt_at,
            status.last_success_at,
        ) {
            return;
        }
        let Ok(_guard) = self.report_send_lock.try_lock() else {
            return;
        };
        if let Err(error) = self.deliver_email_report(&settings).await {
            tracing::warn!(error = %error, "scheduled report email failed");
        }
    }

    async fn deliver_email_report(
        &self,
        settings: &EmailReportSettings,
    ) -> Result<EmailDeliveryResult> {
        let dashboard = self.dashboard().await?;
        match send_report(settings, &dashboard).await {
            Ok(result) => {
                self.store
                    .record_report_delivery(true, result.recipient_count, None)?;
                Ok(result)
            }
            Err(error) => {
                if let Err(store_error) = self.store.record_report_delivery(
                    false,
                    settings.recipients.len(),
                    Some(&error.to_string()),
                ) {
                    tracing::warn!(error = %store_error, "failed to record report delivery error");
                }
                Err(error)
            }
        }
    }
}
