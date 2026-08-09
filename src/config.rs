use std::{
    env,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const DEFAULT_KEYCHAIN_SERVICE: &str = "io.github.metasys-summary.dashboard";

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorPreference {
    Auto,
    Rest,
    Legacy,
    Demo,
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
        let password = env_string("METASYS_PASSWORD").or_else(|| {
            keyring::Entry::new(&keychain_service, &username)
                .ok()
                .and_then(|entry| entry.get_password().ok())
        });
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
            .or(file.database_path)
            .unwrap_or_else(|| default_data_dir.join("dashboard.sqlite3"));

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
        if let Some(parent) = self.database_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create data directory {}", parent.display()))?;
        }
        Ok(())
    }
}

pub fn store_password(service: &str, username: &str, password: &str) -> Result<()> {
    let entry = keyring::Entry::new(service, username).context("open macOS Keychain")?;
    entry
        .set_password(password)
        .context("save password in macOS Keychain")
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
    use super::ConnectorPreference;

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
}
