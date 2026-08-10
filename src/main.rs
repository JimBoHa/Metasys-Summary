use std::{collections::BTreeSet, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use metasys_dashboard::{
    app::AppState,
    config::{Config, ConnectorPreference, load_password, store_password},
    metasys::MetasysClient,
    portal::{auth::hash_password, models::PortalRole},
    sql_trends::{
        FEATURED_TREND_POINT_FAMILIES, SqlTrendSettings, fetch_trend_points, fetch_trends,
        set_sql_password, test_connection,
    },
    store::Store,
    web,
};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(author, version, about)]
struct Cli {
    /// TOML configuration file. Defaults to Application Support directory.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Run with clearly labeled generated data.
    #[arg(long, global = true)]
    demo: bool,

    /// Open dashboard in default browser after startup.
    #[arg(long, global = true)]
    open_browser: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Store Metasys password securely in macOS Keychain.
    Configure {
        #[arg(long)]
        username: Option<String>,
        #[arg(long)]
        keychain_service: Option<String>,
    },
    /// Verify one Metasys poll without starting web server.
    Check,
    /// Create an administrator for the maintenance request portal.
    PortalAdmin {
        #[arg(long)]
        email: String,
        #[arg(long)]
        name: String,
    },
    /// Store a read-only SQL trend connection and password locally.
    ConfigureSql {
        #[arg(long)]
        host: String,
        #[arg(long, default_value = "1433")]
        port: u16,
        #[arg(long)]
        database: String,
        #[arg(long)]
        username: String,
        #[arg(long)]
        trust_server_certificate: bool,
        #[arg(long)]
        legacy_tls: bool,
    },
    /// Verify the saved SQL connection, point catalog, and one read-only trend query.
    CheckSql,
    /// Read one live Metasys point while configuring a portal region.
    CheckTemperature {
        #[arg(long)]
        reference: String,
        #[arg(long, default_value = "85")]
        attribute: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("metasys_dashboard=info,tower_http=info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();
    let mut config = Config::load(cli.config.as_deref(), cli.demo)?;
    match cli.command {
        Some(Command::Configure {
            username,
            keychain_service,
        }) => configure(
            username.unwrap_or_else(|| config.username.clone()),
            keychain_service.unwrap_or(config.keychain_service),
        ),
        Some(Command::Check) => {
            config.hydrate_password();
            check(config).await
        }
        Some(Command::PortalAdmin { email, name }) => create_portal_admin(config, email, name),
        Some(Command::ConfigureSql {
            host,
            port,
            database,
            username,
            trust_server_certificate,
            legacy_tls,
        }) => configure_sql(
            config,
            host,
            port,
            database,
            username,
            trust_server_certificate,
            legacy_tls,
        ),
        Some(Command::CheckSql) => check_sql(config).await,
        Some(Command::CheckTemperature {
            reference,
            attribute,
        }) => {
            config.hydrate_password();
            check_temperature(config, reference, attribute).await
        }
        None => serve(config, cli.open_browser || launched_from_app_bundle()).await,
    }
}

async fn check_sql(config: Config) -> Result<()> {
    config.ensure_data_directory()?;
    let store = Store::open(&config.database_path)?;
    let settings = store.sql_trend_settings()?;
    if !settings.enabled {
        bail!("SQL trend source is disabled");
    }

    test_connection(&settings).await?;
    let catalog = fetch_trend_points(&settings).await?;
    let mut probes = FEATURED_TREND_POINT_FAMILIES
        .iter()
        .filter_map(|family| {
            catalog
                .points
                .iter()
                .find(|point| {
                    point.point_family == *family
                        && point.equipment_name.to_ascii_uppercase().starts_with("TB")
                })
                .or_else(|| {
                    catalog
                        .points
                        .iter()
                        .find(|point| point.point_family == *family)
                })
                .map(|point| (*family, point))
        })
        .take(8)
        .collect::<Vec<_>>();
    if probes.is_empty() {
        probes.push((
            "UNCLASSIFIED",
            catalog
                .points
                .first()
                .context("SQL historian point catalog is empty")?,
        ));
    }
    let point_slice_ids = probes
        .iter()
        .map(|(_, point)| point.point_slice_id)
        .collect::<Vec<_>>();
    let trends = fetch_trends(&settings, 24 * 365 * 5, &point_slice_ids).await?;
    println!(
        "SQL trend check OK: {} catalog points | {} samples from {} read-only five-year probes{}",
        catalog.points.len(),
        trends.sample_count,
        probes.len(),
        if catalog.truncated {
            " | catalog truncated"
        } else {
            ""
        }
    );
    for family in FEATURED_TREND_POINT_FAMILIES {
        let matching = catalog
            .points
            .iter()
            .filter(|point| point.point_family == *family)
            .collect::<Vec<_>>();
        let equipment = matching
            .iter()
            .map(|point| point.equipment_path.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let terminal_boxes = matching
            .iter()
            .filter(|point| point.equipment_name.to_ascii_uppercase().starts_with("TB"))
            .count();
        if let Some((_, probe)) = probes
            .iter()
            .find(|(probe_family, _)| probe_family == family)
        {
            let series = trends
                .series
                .iter()
                .find(|series| series.name == probe.point_name);
            let samples = series.map_or(0, |series| series.samples.len());
            let span = series
                .and_then(|series| Some((series.samples.first()?, series.samples.last()?)))
                .map_or_else(
                    || "no samples returned".to_owned(),
                    |(first, last)| {
                        format!(
                            "{} to {}",
                            first.timestamp.date_naive(),
                            last.timestamp.date_naive()
                        )
                    },
                );
            println!(
                "  {family}: {} points across {equipment} equipment ({terminal_boxes} terminal boxes) | probe {} | {samples} samples ({span})",
                matching.len(),
                probe.point_name,
            );
        } else {
            println!(
                "  {family}: {} points across {equipment} equipment ({terminal_boxes} terminal boxes)",
                matching.len()
            );
        }
    }
    for marker in ["TB", "FAV", "VAV", "WSHP"] {
        let matching = catalog
            .points
            .iter()
            .filter(|point| {
                let equipment = point.equipment_name.to_ascii_uppercase();
                equipment.starts_with(marker) || equipment.contains(&format!("-{marker}"))
            })
            .collect::<Vec<_>>();
        let examples = matching
            .iter()
            .take(4)
            .map(|point| point.point_name.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        println!(
            "  {marker} equipment references: {}{}",
            matching.len(),
            if examples.is_empty() {
                String::new()
            } else {
                format!(" | {examples}")
            }
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn configure_sql(
    config: Config,
    host: String,
    port: u16,
    database: String,
    username: String,
    trust_server_certificate: bool,
    legacy_tls: bool,
) -> Result<()> {
    let settings = SqlTrendSettings {
        enabled: true,
        host,
        port,
        database,
        username,
        trust_server_certificate,
        legacy_tls,
        ..Default::default()
    };
    settings.validate()?;
    let password =
        rpassword::prompt_password(format!("SQL Server password for {}: ", settings.username))?;
    if password.is_empty() {
        bail!("password cannot be empty");
    }
    let confirmation = rpassword::prompt_password("Confirm SQL Server password: ")?;
    if password != confirmation {
        bail!("passwords do not match");
    }
    config.ensure_data_directory()?;
    let store = Store::open(&config.database_path)?;
    set_sql_password(&password)?;
    store.save_sql_trend_settings(&settings)?;
    println!(
        "Read-only SQL trend connection saved locally for {}.",
        settings.username
    );
    Ok(())
}

async fn check_temperature(config: Config, reference: String, attribute: String) -> Result<()> {
    let client = MetasysClient::new(Arc::new(config))?;
    let reading = client.read_temperature(&reference, &attribute).await?;
    println!(
        "Temperature OK: {} {} | {}",
        reading.display_value, reading.unit, reading.status
    );
    Ok(())
}

fn create_portal_admin(config: Config, email: String, display_name: String) -> Result<()> {
    metasys_dashboard::portal::auth::validate_user(&email, &display_name)?;
    let password = rpassword::prompt_password(format!("Portal password for {email}: "))?;
    let confirmation = rpassword::prompt_password("Confirm portal password: ")?;
    if password != confirmation {
        bail!("passwords do not match");
    }
    let password_hash = hash_password(&password)?;
    config.ensure_data_directory()?;
    let store = Store::open(&config.database_path)?;
    let user = store.create_portal_user(
        &email,
        &display_name,
        PortalRole::Admin,
        &password_hash,
        &[],
        &[],
    )?;
    println!("Portal administrator created for {}.", user.email);
    Ok(())
}

fn configure(username: String, service: String) -> Result<()> {
    let password = rpassword::prompt_password(format!("Metasys password for {username}: "))?;
    if password.is_empty() {
        bail!("password cannot be empty");
    }
    let confirmation = rpassword::prompt_password("Confirm password: ")?;
    if password != confirmation {
        bail!("passwords do not match");
    }
    store_password(&service, &username, &password)?;
    println!("Password saved in macOS Keychain for {username}.");
    Ok(())
}

async fn check(config: Config) -> Result<()> {
    let client = MetasysClient::new(Arc::new(config))?;
    let data = client.fetch().await?;
    println!(
        "Connection OK: {} | {} alarm records | {} active | {} overrides",
        data.connector,
        data.alarms.len(),
        data.active_alarms.len(),
        data.overrides.len()
    );
    Ok(())
}

async fn serve(mut config: Config, cli_open_browser: bool) -> Result<()> {
    if cli_open_browser {
        config.open_browser = true;
    }
    config.ensure_data_directory()?;
    let store = Arc::new(Store::open(&config.database_path)?);
    if let Some(settings) = store.metasys_connection_settings()? {
        settings.validate()?;
        let password = config.password.take();
        config.apply_metasys_connection(&settings, password);
    }
    let startup_keychain = (config.password.is_none()
        && config.connector != ConnectorPreference::Demo)
        .then(|| (config.keychain_service.clone(), config.username.clone()));
    let config = Arc::new(config);
    let state = Arc::new(AppState::new(config.clone(), store)?);

    let address = (config.bind_address, config.port);
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("bind dashboard to {}:{}", address.0, address.1))?;

    let startup_poll_state = state.clone();
    tokio::spawn(async move {
        if let Some((service, username)) = startup_keychain {
            let lookup_service = service.clone();
            let lookup_username = username.clone();
            if let Ok(Some(password)) = tokio::task::spawn_blocking(move || {
                load_password(&lookup_service, &lookup_username)
            })
            .await
                && let Err(error) = startup_poll_state
                    .hydrate_metasys_password(&service, &username, password)
                    .await
            {
                tracing::warn!(error = %error, "could not activate Keychain Metasys credential");
            }
        }
        startup_poll_state.poll_once().await;
    });

    let poll_state = state.clone();
    let poll_interval = config.poll_interval_seconds;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(poll_interval));
        interval.tick().await;
        loop {
            interval.tick().await;
            poll_state.poll_once().await;
        }
    });

    let report_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.tick().await;
        loop {
            interval.tick().await;
            report_state.send_scheduled_email_report().await;
        }
    });

    let local_url = format!("http://127.0.0.1:{}", config.port);
    tracing::info!(url = %local_url, bind = %listener.local_addr()?, "Metasys dashboard ready");
    println!("Metasys Dashboard: {local_url}");
    println!("LAN access: http://<this-mac-ip>:{}", config.port);

    if config.open_browser {
        let browser_url = local_url.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = std::process::Command::new("open").arg(browser_url).status() {
                tracing::warn!(error = %error, "could not open browser");
            }
        });
    }

    axum::serve(
        listener,
        web::router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("run dashboard web server")
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_ok() {
        tracing::info!("shutdown requested");
    }
}

fn launched_from_app_bundle() -> bool {
    std::env::current_exe()
        .ok()
        .is_some_and(|path| path.to_string_lossy().contains(".app/Contents/MacOS/"))
}
