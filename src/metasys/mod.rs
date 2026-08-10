mod demo;
mod legacy;
mod modern;

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
    time::Duration as StdDuration,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeZone, Utc};
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::{
    config::{Config, ConnectorPreference},
    models::{AlarmRecord, PollData},
    portal::models::TemperatureReading,
};

#[derive(Clone, Debug, PartialEq, Eq)]
enum ResolvedConnector {
    Modern { version: String },
    Legacy,
}

#[derive(Clone)]
struct AuthSession {
    connector: ResolvedConnector,
    token: String,
    expires_at: DateTime<Utc>,
    client_id: Option<String>,
    authorization_data: Option<String>,
}

#[derive(Default)]
struct RuntimeState {
    connector: Option<ResolvedConnector>,
    session: Option<AuthSession>,
}

pub struct MetasysClient {
    config: Arc<Config>,
    http: Client,
    state: Mutex<RuntimeState>,
    legacy_signalr: Mutex<Option<legacy::LegacySignalrConnection>>,
}

impl MetasysClient {
    pub fn new(config: Arc<Config>) -> Result<Self> {
        let http = Client::builder()
            .danger_accept_invalid_certs(config.accept_invalid_certificates)
            .http1_only()
            .timeout(StdDuration::from_secs(60))
            .user_agent(concat!("metasys-dashboard/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("build Metasys HTTP client")?;
        Ok(Self {
            config,
            http,
            state: Mutex::new(RuntimeState::default()),
            legacy_signalr: Mutex::new(None),
        })
    }

    pub async fn fetch(&self) -> Result<PollData> {
        if self.config.connector == ConnectorPreference::Demo {
            return Ok(demo::poll_data());
        }
        if self.config.password.is_none() {
            bail!(
                "Metasys password is missing. Run `metasys-dashboard configure` or set METASYS_PASSWORD"
            );
        }

        let connector = self.resolve_connector().await?;
        let session = self.ensure_session(&connector).await?;
        let result = match &session.connector {
            ResolvedConnector::Modern { version } => {
                modern::fetch(&self.http, &self.config, &session.token, version).await
            }
            ResolvedConnector::Legacy => legacy::fetch(&self.http, &self.config, &session).await,
        };
        if result.is_err() {
            self.state.lock().await.session = None;
            self.legacy_signalr.lock().await.take();
        }
        result
    }

    pub async fn read_temperature(
        &self,
        object_id: &str,
        attribute_id: &str,
    ) -> Result<TemperatureReading> {
        if self.config.password.is_none() {
            bail!("Metasys password is missing");
        }
        if object_id.trim().is_empty() {
            bail!("temperature point is not mapped");
        }
        let connector = self.resolve_connector().await?;
        let session = self.ensure_session(&connector).await?;
        match &session.connector {
            ResolvedConnector::Modern { version } => {
                modern::read_temperature(
                    &self.http,
                    &self.config,
                    &session.token,
                    version,
                    object_id,
                    attribute_id,
                )
                .await
            }
            ResolvedConnector::Legacy => {
                let mut signalr = self.legacy_signalr.lock().await;
                legacy::read_temperature(
                    &self.http,
                    &self.config,
                    &session,
                    object_id,
                    attribute_id,
                    &mut signalr,
                )
                .await
            }
        }
    }

    async fn resolve_connector(&self) -> Result<ResolvedConnector> {
        if let Some(connector) = self.state.lock().await.connector.clone() {
            return Ok(connector);
        }

        let connector = match self.config.connector {
            ConnectorPreference::Rest => ResolvedConnector::Modern {
                version: self.config.api_version.clone(),
            },
            ConnectorPreference::Legacy => ResolvedConnector::Legacy,
            ConnectorPreference::Auto => {
                let modern_login = format!("{}/api/login", self.config.server_url);
                let status = self
                    .http
                    .get(&modern_login)
                    .send()
                    .await
                    .with_context(|| format!("probe modern Metasys API at {modern_login}"))?
                    .status();
                if status != StatusCode::NOT_FOUND {
                    ResolvedConnector::Modern {
                        version: self.config.api_version.clone(),
                    }
                } else {
                    let legacy_url = format!(
                        "{}/UI/api/Authentication/GetPreLoginState",
                        self.config.server_url
                    );
                    let response =
                        self.http.get(&legacy_url).send().await.with_context(|| {
                            format!("probe legacy Metasys UI API at {legacy_url}")
                        })?;
                    if response.status().is_success() {
                        ResolvedConnector::Legacy
                    } else {
                        bail!(
                            "no supported Metasys API found (modern /api/login returned {status}; legacy UI returned {})",
                            response.status()
                        );
                    }
                }
            }
            ConnectorPreference::Demo => unreachable!("demo connector handled before resolution"),
        };
        self.state.lock().await.connector = Some(connector.clone());
        Ok(connector)
    }

    async fn ensure_session(&self, connector: &ResolvedConnector) -> Result<AuthSession> {
        if let Some(session) = self.state.lock().await.session.clone()
            && &session.connector == connector
            && session.expires_at > Utc::now() + chrono::Duration::minutes(2)
        {
            return Ok(session);
        }

        let mut session = match connector {
            ResolvedConnector::Modern { .. } => modern::login(&self.http, &self.config).await?,
            ResolvedConnector::Legacy => legacy::login(&self.http, &self.config).await?,
        };

        let resolved = match connector {
            ResolvedConnector::Modern { version } if version.eq_ignore_ascii_case("auto") => {
                let detected =
                    modern::detect_version(&self.http, &self.config, &session.token).await?;
                ResolvedConnector::Modern { version: detected }
            }
            other => other.clone(),
        };
        session.connector = resolved.clone();
        let mut state = self.state.lock().await;
        state.connector = Some(resolved);
        state.session = Some(session.clone());
        Ok(session)
    }
}

fn parse_datetime(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let value = value?;
    if let Some(timestamp) = value.as_str() {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(timestamp) {
            return Some(parsed.with_timezone(&Utc));
        }
        if let Some(milliseconds) = timestamp
            .strip_prefix("/Date(")
            .and_then(|value| value.split(['+', '-', ')']).next())
            .and_then(|value| value.parse::<i64>().ok())
        {
            return Utc.timestamp_millis_opt(milliseconds).single();
        }
    }
    let numeric = value.as_i64()?;
    if numeric.abs() > 10_000_000_000 {
        Utc.timestamp_millis_opt(numeric).single()
    } else {
        Utc.timestamp_opt(numeric, 0).single()
    }
}

fn first_string(value: &Value, pointers: &[&str]) -> Option<String> {
    pointers
        .iter()
        .filter_map(|pointer| value.pointer(pointer))
        .find_map(value_to_optional_string)
}

fn first_u64(value: &Value, pointers: &[&str]) -> Option<u64> {
    pointers.iter().find_map(|pointer| {
        let item = value.pointer(pointer)?;
        item.as_u64().or_else(|| item.as_str()?.parse::<u64>().ok())
    })
}

fn first_f64(value: &Value, pointers: &[&str]) -> Option<f64> {
    pointers.iter().find_map(|pointer| {
        let item = value.pointer(pointer)?;
        item.as_f64().or_else(|| {
            item.as_str()?
                .trim()
                .trim_end_matches(['°', 'F', 'C'])
                .trim()
                .parse::<f64>()
                .ok()
        })
    })
}

fn first_bool(value: &Value, pointers: &[&str]) -> Option<bool> {
    pointers.iter().find_map(|pointer| {
        let item = value.pointer(pointer)?;
        item.as_bool()
            .or_else(|| match item.as_str()?.to_ascii_lowercase().as_str() {
                "true" | "yes" | "1" => Some(true),
                "false" | "no" | "0" => Some(false),
                _ => None,
            })
    })
}

fn value_to_optional_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) if !value.trim().is_empty() => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Object(map) => map
            .get("Value")
            .or_else(|| map.get("value"))
            .or_else(|| map.get("item"))
            .and_then(value_to_optional_string),
        _ => None,
    }
}

fn value_to_string(value: Option<&Value>) -> String {
    value.and_then(value_to_optional_string).unwrap_or_default()
}

fn stable_id(parts: &[&str]) -> String {
    let mut hasher = DefaultHasher::new();
    parts.hash(&mut hasher);
    format!("generated-{:016x}", hasher.finish())
}

fn is_normal_alarm_type(alarm_type: &str) -> bool {
    let value = alarm_type.to_ascii_lowercase();
    value.ends_with(".avnormal") || value == "normal" || value.ends_with("osnormal")
}

fn deduplicate_alarms(alarms: Vec<AlarmRecord>) -> Vec<AlarmRecord> {
    let mut by_id = std::collections::HashMap::new();
    for alarm in alarms {
        by_id
            .entry(alarm.id.clone())
            .and_modify(|existing: &mut AlarmRecord| {
                if alarm.occurred_at >= existing.occurred_at {
                    *existing = alarm.clone();
                }
            })
            .or_insert(alarm);
    }
    by_id.into_values().collect()
}
