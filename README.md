# Metasys Operations Dashboard

Native Rust service for macOS with a responsive LAN/local web dashboard. It monitors Johnson Controls Metasys alarm activity and operator overrides, persists history in SQLite, and calculates equipment risk locally.

Licensed under the [MIT License](LICENSE).

## Configure

Copy `config.example.toml` to:

```text
~/Library/Application Support/Metasys Dashboard/config.toml
```

Set `server_url` and `username` for your site. Store the password in macOS Keychain; never put it in the TOML file:

```bash
cargo run -- configure
```

The password is never written to this repository, the TOML configuration, SQLite, or logs.

## Run

```bash
cargo run -- check
cargo run
```

Open `http://127.0.0.1:3030`. To allow other local-network devices, set `bind_address = "0.0.0.0"`; then open `http://<this-mac-ip>:3030` after macOS Firewall allows incoming connections.

To preview the complete dashboard without Metasys:

```bash
cargo run -- --demo --open-browser
```

## Build the macOS app

```bash
./scripts/build-macos-app.sh
open "dist/Metasys Dashboard.app"
```

The generated app is an ad-hoc-signed native Apple application bundle. Launching it starts the Rust web service and opens the dashboard in the default browser.

Optional login-time background service:

```bash
./scripts/install-launch-agent.sh
```

## Dashboard sections

- Current active alarms, sorted by Metasys priority (lower numbers are more serious)
- Most frequent alarms in the rolling 30-day index
- Most serious alarms in the rolling 30-day index
- Active operator and timed overrides
- Problematic equipment ranked by alarm volume, active alarms, and severity
- Fourteen-day daily alarm chart with a seven-day rolling mean
- Alarm-type and equipment-share donut charts
- Configurable encrypted email reports with manual, daily, weekday, or weekly delivery
- Optional Microsoft SQL Server trend chart with selectable 24-hour to 30-day ranges

Modern Metasys REST versions v2-v6 are auto-detected. REST v5/v6 uses activities; v2-v4 uses alarm collections with version-appropriate filters. The legacy connector uses Alarm Manager for events and Potential Problem Areas for overrides. SQLite deduplicates event IDs across polls, so the local 30-day index improves continuously even when an older Metasys endpoint returns a limited initial history.

## Email reports

Open **Reports** from a browser running on the host Mac. Configure an SMTP relay, sender, recipient list, local schedule, and report sections. Available sections include active alarms, most common alarms, most serious alarms, operator overrides, problematic equipment, inferred equipment-offline conditions, and the 14-day alarm rate.

Passwords stay in macOS Keychain. Only encrypted SMTP transports are supported: STARTTLS or implicit TLS. Settings, SMTP testing, and manual sending reject non-loopback clients. Scheduled reports run while the dashboard service is running and retry failures no more than once every 15 minutes. See [email report setup](docs/email-reports.md).

## SQL trend source

Open **SQL Trends** from a browser on the host Mac. Configure the SQL Server hostname or IP, port, database, read-only username, password, certificate policy, and mapping query. The password is stored only in macOS Keychain; non-secret settings are stored in the dashboard SQLite database. Settings endpoints reject non-loopback clients.

The mapping query must be one read-only `SELECT` or `WITH` statement, use `@P1` and `@P2` for UTC start/end bounds, and return these aliases:

- `point_name` as text
- `sample_time` as `datetime`, `datetime2`, or `datetimeoffset`
- `sample_value` as a numeric value
- `unit` as text or `NULL`

Metasys repository schemas vary. A stable read-only compatibility view is recommended. See [SQL trend setup](docs/sql-trends.md).

## Environment overrides

| Variable | Purpose |
|---|---|
| `METASYS_SERVER_URL` | Server base URL |
| `METASYS_USERNAME` | Metasys user |
| `METASYS_PASSWORD` | One-run password override; Keychain is preferred |
| `METASYS_DOMAIN` | Legacy login domain, default `Metasys Local` |
| `METASYS_CONNECTOR` | `auto`, `rest`, `legacy`, or `demo` |
| `METASYS_API_VERSION` | `auto`, `v2`, `v3`, `v4`, `v5`, or `v6` |
| `METASYS_BIND_ADDRESS` | `127.0.0.1` for local-only or `0.0.0.0` for LAN |
| `METASYS_PORT` | Dashboard port, default `3030` |
| `METASYS_DATABASE_PATH` | SQLite database location |

## Security and operations

- Connector is read-only. It does not acknowledge/discard alarms, command points, or release overrides.
- LAN dashboard has no separate login. Bind to `127.0.0.1` unless the building network segment is trusted and access-controlled.
- Email configuration and sending are restricted to loopback clients; report recipients and delivery status are not exposed to LAN clients.
- SQL settings and connection testing are restricted to loopback clients. Trend results remain visible wherever the dashboard is visible.
- Certificate validation is enabled by default. Use `accept_invalid_certificates = true` only for an isolated deployment whose private certificate cannot yet be trusted.
- Use an API-access Metasys account when the public REST API add-on is available. Johnson Controls documents that Standard and Tenant access types are rejected by the public REST API.
- Alarm history stays on this Mac in `~/Library/Application Support/Metasys Dashboard/dashboard.sqlite3`.

## Verify

```bash
cargo fmt -- --check
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo build --release
```

Metasys references: [official REST API v4](https://jci-metasys.github.io/api-landing/api/v4), [API version support matrix](https://jci-metasys.github.io/api-landing/guides/version-support-matrix/), and [creating an API user](https://docs.johnsoncontrols.com/bas/r/Metasys/en-US/Security-Administrator-System-Technical-Bulletin/13.0/Detailed-procedures-for-the-Metasys-UI/Creating-a-user-who-can-access-the-Metasys-REST-API).
