use std::{
    collections::BTreeSet,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use metasys_dashboard::{
    app::AppState,
    config::{Config, ConnectorPreference, load_password, store_password},
    history::HistoryStore,
    history_migration::migrate_sqlite_history,
    metasys::MetasysClient,
    portal::{auth::hash_password, models::PortalRole},
    sql_mirror::{inspect_configured_sql_mirror, inspect_sql_mirror},
    sql_trends::{
        FEATURED_TREND_POINT_FAMILIES, SqlTrendSettings, fetch_live_point_values,
        fetch_trend_points, fetch_trends, inspect_historian_database, mirror_historian_database,
        set_sql_password, test_connection,
    },
    store::Store,
    web,
};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

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
    /// Print read-only SQL historian table, column, and index metadata as JSON.
    InspectSqlHistory,
    /// Mirror the complete SQL historian with resumable checkpoints on marked external storage.
    MirrorSqlHistory {
        /// DuckDB file on the external storage volume.
        #[arg(long)]
        target: PathBuf,
        /// Marker file proving the intended external volume is mounted.
        #[arg(long)]
        volume_marker: PathBuf,
        /// Rows committed per resumable event-data transaction.
        #[arg(long, default_value = "100000")]
        batch_size: usize,
        /// Stop after this many rows in each large history stream; intended for validation.
        #[arg(long)]
        max_event_rows: Option<u64>,
    },
    /// Check DuckDB mirror integrity and resumable-copy progress without changing it.
    CheckSqlMirror {
        #[arg(long)]
        target: PathBuf,
    },
    /// Run the configured SQL mirror when its saved cadence is due.
    RunScheduledSqlMirror {
        /// Run now even if the configured cadence is not due.
        #[arg(long)]
        force: bool,
    },
    /// Verify the local DuckDB history schema and report stored row counts.
    CheckHistory,
    /// Copy legacy alarm and poll history from SQLite into DuckDB.
    MigrateHistory {
        /// Legacy SQLite source; defaults to the configured operational database.
        #[arg(long)]
        source: Option<PathBuf>,
        /// DuckDB target; defaults to the configured history database.
        #[arg(long)]
        target: Option<PathBuf>,
        /// Validate and report rows without creating or changing DuckDB.
        #[arg(long)]
        dry_run: bool,
    },
    /// List saved SQL historian points, optionally filtering by reference text.
    ListSqlPoints {
        #[arg(long)]
        contains: Option<String>,
    },
    /// Replace the saved equipment hierarchy from a validated JSON inventory.
    ImportInventory {
        #[arg(long)]
        file: PathBuf,
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
        Some(Command::InspectSqlHistory) => inspect_sql_history(config).await,
        Some(Command::MirrorSqlHistory {
            target,
            volume_marker,
            batch_size,
            max_event_rows,
        }) => mirror_sql_history(config, target, volume_marker, batch_size, max_event_rows).await,
        Some(Command::CheckSqlMirror { target }) => check_sql_mirror(target),
        Some(Command::RunScheduledSqlMirror { force }) => {
            run_scheduled_sql_mirror(config, force).await
        }
        Some(Command::CheckHistory) => check_history(config),
        Some(Command::MigrateHistory {
            source,
            target,
            dry_run,
        }) => migrate_history(config, source, target, dry_run),
        Some(Command::ListSqlPoints { contains }) => list_sql_points(config, contains).await,
        Some(Command::ImportInventory { file }) => import_inventory(config, file),
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

async fn inspect_sql_history(config: Config) -> Result<()> {
    config.ensure_data_directory()?;
    let store = Store::open(&config.database_path)?;
    let settings = store.sql_trend_settings()?;
    let inspection = inspect_historian_database(&settings).await?;
    println!("{}", serde_json::to_string_pretty(&inspection)?);
    Ok(())
}

async fn mirror_sql_history(
    config: Config,
    target: PathBuf,
    volume_marker: PathBuf,
    batch_size: usize,
    max_event_rows: Option<u64>,
) -> Result<()> {
    config.ensure_data_directory()?;
    let store = Store::open(&config.database_path)?;
    let settings = store.sql_trend_settings()?;
    let report = mirror_historian_database(
        &settings,
        &target,
        &volume_marker,
        batch_size,
        max_event_rows,
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn check_sql_mirror(target: PathBuf) -> Result<()> {
    let status = inspect_sql_mirror(&target)?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    if !status.checkpoints_match_storage {
        bail!("DuckDB mirror checkpoint totals do not match stored event rows");
    }
    if !status.reporting_checkpoints_match_storage {
        bail!("DuckDB reporting checkpoint totals do not match stored rows");
    }
    if !status.operational_snapshot_counts_cover_source {
        bail!("one or more mirrored SQL tables do not cover their captured source row counts");
    }
    Ok(())
}

async fn run_scheduled_sql_mirror(config: Config, force: bool) -> Result<()> {
    config.ensure_data_directory()?;
    let store = Store::open(&config.database_path)?;
    let now = chrono::Utc::now();
    let interrupted = store.mark_interrupted_sql_mirror_runs(now)?;
    if interrupted > 0 {
        tracing::warn!(interrupted, "recorded incomplete SQL mirror scheduler runs");
    }
    let mirror_settings = store.sql_mirror_settings()?;
    let latest = store.recent_sql_mirror_runs(1)?.into_iter().next();
    if !mirror_settings.enabled {
        println!("SQL mirror schedule is disabled");
        return Ok(());
    }
    if !force && !mirror_settings.is_due(now, latest.as_ref()) {
        println!("SQL mirror schedule is not due");
        return Ok(());
    }

    let run_id = Uuid::new_v4().to_string();
    store.begin_sql_mirror_run(&run_id, &mirror_settings, now)?;
    let started = Instant::now();
    let result = async {
        let sql_settings = store.sql_trend_settings()?;
        let target_database = PathBuf::from(&mirror_settings.target_database);
        let volume_marker = PathBuf::from(&mirror_settings.volume_marker);
        let report = mirror_historian_database(
            &sql_settings,
            &target_database,
            &volume_marker,
            mirror_settings.batch_size,
            None,
        )
        .await?;
        let status = inspect_configured_sql_mirror(&mirror_settings)?;
        Ok::<_, anyhow::Error>((report, status))
    }
    .await;
    let finished_at = chrono::Utc::now();
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;

    match result {
        Ok((report, status)) => {
            let integrity_ok = status.checkpoints_match_storage
                && status.reporting_checkpoints_match_storage
                && status.operational_snapshot_counts_cover_source;
            let error_message = (!integrity_ok)
                .then_some("SQL mirror completed, but its post-run integrity check failed");
            store.finish_sql_mirror_run(
                &run_id,
                if integrity_ok { "succeeded" } else { "failed" },
                finished_at,
                duration_ms,
                Some(report.event_rows_copied_this_run),
                Some(report.event_rows_copied_total),
                Some(report.source_event_rows_at_start),
                Some(status.total_mirrored_rows),
                Some(integrity_ok),
                error_message,
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !integrity_ok {
                bail!("SQL mirror post-run integrity check failed");
            }
            Ok(())
        }
        Err(error) => {
            let details = format!("{error:#}");
            store.finish_sql_mirror_run(
                &run_id,
                "failed",
                finished_at,
                duration_ms,
                None,
                None,
                None,
                None,
                None,
                Some(&details),
            )?;
            Err(error)
        }
    }
}

fn import_inventory(config: Config, file: PathBuf) -> Result<()> {
    config.ensure_data_directory()?;
    let encoded = std::fs::read_to_string(&file)
        .with_context(|| format!("read equipment inventory {}", file.display()))?;
    let inventory =
        serde_json::from_str::<metasys_dashboard::inventory::EquipmentInventory>(&encoded)
            .with_context(|| format!("decode equipment inventory {}", file.display()))?;
    inventory.validate()?;
    let store = Store::open(&config.database_path)?;
    store.replace_equipment_inventory(&inventory)?;
    println!(
        "Equipment inventory imported: {} groups | {} equipment | {} points",
        inventory.groups.len(),
        inventory.equipment_count(),
        inventory.point_count()
    );
    Ok(())
}

fn check_history(config: Config) -> Result<()> {
    config.ensure_data_directory()?;
    let history = HistoryStore::open(&config.history_database_path)?;
    let summary = history.summary()?;
    println!(
        "DuckDB history OK: schema {} | {} point samples | {} alarm events | {} poll runs | {} legacy imports | {}",
        summary.schema_version,
        summary.point_samples,
        summary.alarm_events,
        summary.poll_runs,
        summary.legacy_imports,
        history.path().display(),
    );
    Ok(())
}

fn migrate_history(
    config: Config,
    source: Option<PathBuf>,
    target: Option<PathBuf>,
    dry_run: bool,
) -> Result<()> {
    let source = source.unwrap_or_else(|| config.database_path.clone());
    let target = target.unwrap_or_else(|| config.history_database_path.clone());
    let report = migrate_sqlite_history(&source, &target, dry_run)?;
    println!(
        "{}: {} alarm rows | {} poll rows | source fingerprint {}{}",
        if dry_run {
            "DuckDB migration dry run OK"
        } else if report.already_imported {
            "DuckDB migration already applied"
        } else {
            "DuckDB migration complete"
        },
        report.alarm_rows,
        report.poll_rows,
        report.source_fingerprint,
        if dry_run {
            String::new()
        } else {
            format!(" | {}", report.target.display())
        }
    );
    Ok(())
}

async fn list_sql_points(config: Config, contains: Option<String>) -> Result<()> {
    config.ensure_data_directory()?;
    let store = Store::open(&config.database_path)?;
    let settings = store.sql_trend_settings()?;
    if !settings.enabled {
        bail!("SQL trend source is disabled");
    }
    let catalog = fetch_trend_points(&settings).await?;
    let contains = contains.map(|value| value.to_ascii_lowercase());
    for point in catalog.points.iter().filter(|point| {
        contains
            .as_ref()
            .is_none_or(|value| point.point_name.to_ascii_lowercase().contains(value))
    }) {
        println!(
            "{}\t{}\t{}\t{}\t{}",
            point.point_slice_id,
            point.point_name,
            point.unit.as_deref().unwrap_or(""),
            point.equipment_name,
            point.point_family,
        );
    }
    Ok(())
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
    let inventory_live_probe = store.equipment_inventory()?.and_then(|inventory| {
        inventory
            .groups
            .into_iter()
            .flat_map(|group| group.equipment)
            .filter(|equipment| equipment.name.to_ascii_uppercase().starts_with("TB2-2"))
            .find_map(|equipment| {
                let ids = equipment
                    .points
                    .into_iter()
                    .filter_map(|point| point.historian_point_slice_id)
                    .collect::<Vec<_>>();
                (!ids.is_empty()).then_some((equipment.name, ids))
            })
    });
    let (live_probe_name, live_point_slice_ids) = inventory_live_probe.unwrap_or_else(|| {
        (
            "featured historian probes".to_owned(),
            point_slice_ids.clone(),
        )
    });
    let live_values = fetch_live_point_values(&settings, &live_point_slice_ids).await?;
    let trends = fetch_trends(&settings, 24 * 365 * 5, &point_slice_ids).await?;
    println!(
        "SQL trend check OK: {} catalog points | {} latest values for {} | {} samples from {} read-only five-year probes{}",
        catalog.points.len(),
        live_values.values.len(),
        live_probe_name,
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

    let history_state = state.clone();
    let history_sample_interval = config.history_sample_interval_seconds;
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(history_sample_interval));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match history_state.record_inventory_point_snapshot().await {
                Ok(recorded) if recorded > 0 => {
                    tracing::info!(recorded, "recorded historian point samples in DuckDB");
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(error = %error, "DuckDB historian snapshot failed");
                }
            }
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
