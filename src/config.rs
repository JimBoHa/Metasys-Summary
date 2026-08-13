use std::{
    env,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const DEFAULT_KEYCHAIN_SERVICE: &str = "io.github.metasys-summary.dashboard";
pub const BROWSER_KEYCHAIN_SERVICE: &str = "io.github.metasys-summary.dashboard.metasys";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorPreference {
    Auto,
    Rest,
    Legacy,
    Demo,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetasysConnectionSettings {
    pub server_url: String,
    pub username: String,
    pub domain: String,
    pub connector: ConnectorPreference,
    pub api_version: String,
    pub accept_invalid_certificates: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetasysConnectionUpdate {
    pub server_url: String,
    pub username: String,
    pub password: String,
    pub password_confirmation: String,
    pub domain: String,
    pub connector: ConnectorPreference,
    pub api_version: String,
    pub accept_invalid_certificates: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetasysConnectionView {
    pub server_url: String,
    pub username: String,
    pub domain: String,
    pub connector: ConnectorPreference,
    pub api_version: String,
    pub accept_invalid_certificates: bool,
    pub password_configured: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetasysConnectionResult {
    pub settings: MetasysConnectionView,
    pub connector: String,
    pub server_version: Option<String>,
    pub alarm_records: usize,
    pub active_alarms: usize,
    pub overrides: usize,
}

impl MetasysConnectionUpdate {
    pub fn validated(&self) -> Result<(MetasysConnectionSettings, String)> {
        if self.password != self.password_confirmation {
            bail!("password confirmation does not match");
        }
        let password_length = self.password.chars().count();
        if !(1..=1_024).contains(&password_length) {
            bail!("Metasys password must contain 1 to 1,024 characters");
        }
        let settings = MetasysConnectionSettings {
            server_url: self.server_url.trim().trim_end_matches('/').to_owned(),
            username: self.username.trim().to_owned(),
            domain: self.domain.trim().to_owned(),
            connector: self.connector,
            api_version: self.api_version.trim().to_ascii_lowercase(),
            accept_invalid_certificates: self.accept_invalid_certificates,
        };
        settings.validate()?;
        Ok((settings, self.password.clone()))
    }
}

impl MetasysConnectionSettings {
    pub fn validate(&self) -> Result<()> {
        if self.server_url.is_empty() || self.server_url.len() > 2_048 {
            bail!("Metasys server URL must contain 1 to 2,048 characters");
        }
        let url = reqwest::Url::parse(&self.server_url).context("enter a valid Metasys URL")?;
        if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
            bail!("Metasys URL must use http or https and include a host");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("Metasys URL must not contain credentials");
        }
        if url.query().is_some() || url.fragment().is_some() {
            bail!("Metasys URL must not contain a query or fragment");
        }
        validate_connection_field("username", &self.username, 256)?;
        validate_connection_field("domain", &self.domain, 128)?;
        if self.connector == ConnectorPreference::Demo {
            bail!("demo data cannot be selected for a production connection");
        }
        if !matches!(
            self.api_version.as_str(),
            "auto" | "v2" | "v3" | "v4" | "v5" | "v6"
        ) {
            bail!("API version must be auto, v2, v3, v4, v5, or v6");
        }
        Ok(())
    }

    pub fn view(&self, password_configured: bool) -> MetasysConnectionView {
        MetasysConnectionView {
            server_url: self.server_url.clone(),
            username: self.username.clone(),
            domain: self.domain.clone(),
            connector: self.connector,
            api_version: self.api_version.clone(),
            accept_invalid_certificates: self.accept_invalid_certificates,
            password_configured,
        }
    }
}

fn validate_connection_field(label: &str, value: &str, maximum: usize) -> Result<()> {
    let length = value.chars().count();
    if length == 0 || length > maximum || value.chars().any(char::is_control) {
        bail!("Metasys {label} must contain 1 to {maximum} characters");
    }
    Ok(())
}

impl ConnectorPreference {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "rest" | "modern" => Ok(Self::Rest),
            "legacy" | "ui" => Ok(Self::Legacy),
            "demo" => Ok(Self::Demo),
            other => bail!("unknown connector '{other}'; expected auto, rest, legacy, or demo"),
        }
    }
}

#[derive(Clone)]
pub struct Config {
    pub server_url: String,
    pub username: String,
    pub password: Option<String>,
    pub domain: String,
    pub connector: ConnectorPreference,
    pub api_version: String,
    pub bind_address: IpAddr,
    pub port: u16,
    pub poll_interval_seconds: u64,
    pub history_days: i64,
    pub database_path: PathBuf,
    pub history_database_path: PathBuf,
    pub history_sample_interval_seconds: u64,
    pub accept_invalid_certificates: bool,
    pub open_browser: bool,
    pub keychain_service: String,
    pub max_alarm_records: usize,
    pub max_override_points: usize,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct FileConfig {
    server_url: Option<String>,
    username: Option<String>,
    domain: Option<String>,
    connector: Option<ConnectorPreference>,
    api_version: Option<String>,
    bind_address: Option<IpAddr>,
    port: Option<u16>,
    poll_interval_seconds: Option<u64>,
    history_days: Option<i64>,
    database_path: Option<PathBuf>,
    history_database_path: Option<PathBuf>,
    history_sample_interval_seconds: Option<u64>,
    accept_invalid_certificates: Option<bool>,
    open_browser: Option<bool>,
    keychain_service: Option<String>,
    max_alarm_records: Option<usize>,
    max_override_points: Option<usize>,
}

impl Config {
    pub fn load(explicit_path: Option<&Path>, force_demo: bool) -> Result<Self> {
        let default_data_dir = application_support_dir();
        let default_config_path = default_data_dir.join("config.toml");
        let config_path = explicit_path
            .map(PathBuf::from)
            .or_else(|| env::var_os("METASYS_CONFIG").map(PathBuf::from))
            .unwrap_or(default_config_path);

        let file = if config_path.exists() {
            let raw = std::fs::read_to_string(&config_path)
                .with_context(|| format!("read config {}", config_path.display()))?;
            toml::from_str::<FileConfig>(&raw)
                .with_context(|| format!("parse config {}", config_path.display()))?
        } else {
            FileConfig::default()
        };

        let server_url = env_string("METASYS_SERVER_URL")
            .or(file.server_url)
            .unwrap_or_else(|| "https://metasys.example.invalid".to_owned());
        let username = env_string("METASYS_USERNAME")
            .or(file.username)
            .unwrap_or_else(|| "metasys-api-user".to_owned());
        let keychain_service = env_string("METASYS_KEYCHAIN_SERVICE")
            .or(file.keychain_service)
            .unwrap_or_else(|| DEFAULT_KEYCHAIN_SERVICE.to_owned());
        let password = env_string("METASYS_PASSWORD");
        let connector = if force_demo {
            ConnectorPreference::Demo
        } else if let Some(value) = env_string("METASYS_CONNECTOR") {
            ConnectorPreference::parse(&value)?
        } else {
            file.connector.unwrap_or(ConnectorPreference::Auto)
        };

        let bind_address = parse_env("METASYS_BIND_ADDRESS")?
            .or(file.bind_address)
            .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let port = parse_env("METASYS_PORT")?.or(file.port).unwrap_or(3030);
        let poll_interval_seconds = parse_env("METASYS_POLL_INTERVAL_SECONDS")?
            .or(file.poll_interval_seconds)
            .unwrap_or(60)
            .max(15);
        let history_days = parse_env("METASYS_HISTORY_DAYS")?
            .or(file.history_days)
            .unwrap_or(30)
            .clamp(14, 365);
        let max_alarm_records = parse_env("METASYS_MAX_ALARM_RECORDS")?
            .or(file.max_alarm_records)
            .unwrap_or(10_000)
            .clamp(100, 100_000);
        let max_override_points = parse_env("METASYS_MAX_OVERRIDE_POINTS")?
            .or(file.max_override_points)
            .unwrap_or(5_000)
            .clamp(100, 50_000);

        let database_path = env::var_os("METASYS_DATABASE_PATH")
            .map(PathBuf::from)
            .or_else(|| force_demo.then(|| default_data_dir.join("dashboard-demo.sqlite3")))
            .or(file.database_path)
            .unwrap_or_else(|| default_data_dir.join("dashboard.sqlite3"));
        let history_database_path = env::var_os("METASYS_HISTORY_DATABASE_PATH")
            .map(PathBuf::from)
            .or_else(|| force_demo.then(|| default_data_dir.join("history-demo.duckdb")))
            .or(file.history_database_path)
            .unwrap_or_else(|| {
                database_path
                    .parent()
                    .unwrap_or(&default_data_dir)
                    .join("history.duckdb")
            });
        let history_sample_interval_seconds = parse_env("METASYS_HISTORY_SAMPLE_INTERVAL_SECONDS")?
            .or(file.history_sample_interval_seconds)
            .unwrap_or(60)
            .clamp(15, 3_600);

        Ok(Self {
            server_url: server_url.trim_end_matches('/').to_owned(),
            username,
            password,
            domain: env_string("METASYS_DOMAIN")
                .or(file.domain)
                .unwrap_or_else(|| "Metasys Local".to_owned()),
            connector,
            api_version: env_string("METASYS_API_VERSION")
                .or(file.api_version)
                .unwrap_or_else(|| "auto".to_owned()),
            bind_address,
            port,
            poll_interval_seconds,
            history_days,
            database_path,
            history_database_path,
            history_sample_interval_seconds,
            accept_invalid_certificates: parse_env("METASYS_ACCEPT_INVALID_CERTIFICATES")?
                .or(file.accept_invalid_certificates)
                .unwrap_or(false),
            open_browser: parse_env("METASYS_OPEN_BROWSER")?
                .or(file.open_browser)
                .unwrap_or(false),
            keychain_service,
            max_alarm_records,
            max_override_points,
        })
    }

    pub fn ensure_data_directory(&self) -> Result<()> {
        for parent in [
            self.database_path.parent(),
            self.history_database_path.parent(),
        ]
        .into_iter()
        .flatten()
        {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create data directory {}", parent.display()))?;
        }
        Ok(())
    }

    pub fn metasys_connection_settings(&self) -> MetasysConnectionSettings {
        MetasysConnectionSettings {
            server_url: self.server_url.clone(),
            username: self.username.clone(),
            domain: self.domain.clone(),
            connector: self.connector,
            api_version: self.api_version.clone(),
            accept_invalid_certificates: self.accept_invalid_certificates,
        }
    }

    pub fn apply_metasys_connection(
        &mut self,
        settings: &MetasysConnectionSettings,
        password: Option<String>,
    ) {
        self.server_url.clone_from(&settings.server_url);
        self.username.clone_from(&settings.username);
        self.domain.clone_from(&settings.domain);
        self.connector = settings.connector;
        self.api_version.clone_from(&settings.api_version);
        self.accept_invalid_certificates = settings.accept_invalid_certificates;
        self.keychain_service = BROWSER_KEYCHAIN_SERVICE.to_owned();
        self.password = password;
    }

    pub fn hydrate_password(&mut self) {
        if self.password.is_none() && self.connector != ConnectorPreference::Demo {
            self.password = load_password(&self.keychain_service, &self.username);
        }
    }
}

pub fn store_password(service: &str, username: &str, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(service, username).context("open macOS Keychain")?;
    entry
        .set_password(password)
        .context("save password in macOS Keychain")
}

pub fn load_password(service: &str, username: &str) -> Option<String> {
    keyring::Entry::new(service, username)
        .ok()
        .and_then(|entry| entry.get_password().ok())
}

fn application_support_dir() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/Application Support/Metasys Dashboard")
}

fn env_string(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn parse_env<T>(name: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    env_string(name)
        .map(|value| {
            value
                .parse::<T>()
                .map(Some)
                .map_err(|error| anyhow::anyhow!("invalid {name} value '{value}': {error}"))
        })
        .transpose()
        .map(Option::flatten)
}

#[cfg(test)]
mod tests {
    use super::{ConnectorPreference, MetasysConnectionUpdate};

    #[test]
    fn connector_aliases_parse() {
        assert_eq!(
            ConnectorPreference::parse("modern").unwrap(),
            ConnectorPreference::Rest
        );
        assert_eq!(
            ConnectorPreference::parse("ui").unwrap(),
            ConnectorPreference::Legacy
        );
    }

    #[test]
    fn validates_browser_connection_without_persisting_password() {
        let update = MetasysConnectionUpdate {
            server_url: "https://metasys.example.test/".to_owned(),
            username: "browser-user".to_owned(),
            password: "external-password".to_owned(),
            password_confirmation: "external-password".to_owned(),
            domain: "Metasys Local".to_owned(),
            connector: ConnectorPreference::Auto,
            api_version: "AUTO".to_owned(),
            accept_invalid_certificates: true,
        };
        let (settings, password) = update.validated().unwrap();
        assert_eq!(settings.server_url, "https://metasys.example.test");
        assert_eq!(settings.api_version, "auto");
        assert_eq!(password, "external-password");
        assert!(
            !serde_json::to_string(&settings)
                .unwrap()
                .contains("external-password")
        );
    }

    #[test]
    fn rejects_demo_and_embedded_url_credentials() {
        let update = MetasysConnectionUpdate {
            server_url: "https://name:secret@metasys.example.test".to_owned(),
            username: "browser-user".to_owned(),
            password: "external-password".to_owned(),
            password_confirmation: "external-password".to_owned(),
            domain: "Metasys Local".to_owned(),
            connector: ConnectorPreference::Demo,
            api_version: "auto".to_owned(),
            accept_invalid_certificates: false,
        };
        assert!(update.validated().is_err());
    }
}
