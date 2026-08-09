use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use metasys_dashboard::{
    app::AppState,
    config::{Config, store_password},
    metasys::MetasysClient,
    portal::{auth::hash_password, models::PortalRole},
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
    let config = Config::load(cli.config.as_deref(), cli.demo)?;
    match cli.command {
        Some(Command::Configure {
            username,
            keychain_service,
        }) => configure(
            username.unwrap_or_else(|| config.username.clone()),
            keychain_service.unwrap_or(config.keychain_service),
        ),
        Some(Command::Check) => check(config).await,
        Some(Command::PortalAdmin { email, name }) => create_portal_admin(config, email, name),
        Some(Command::CheckTemperature {
            reference,
            attribute,
        }) => check_temperature(config, reference, attribute).await,
        None => serve(config, cli.open_browser || launched_from_app_bundle()).await,
    }
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
    let config = Arc::new(config);
    let store = Arc::new(Store::open(&config.database_path)?);
    let state = Arc::new(AppState::new(config.clone(), store)?);

    let poll_state = state.clone();
    let poll_interval = config.poll_interval_seconds;
    tokio::spawn(async move {
        poll_state.poll_once().await;
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

    let address = (config.bind_address, config.port);
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("bind dashboard to {}:{}", address.0, address.1))?;
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
