# SQL trend setup

Metasys Summary can query a Metasys `JCIHistorianDB` repository directly without copying or changing the remote database. The browser loads the point catalog from `tblPoint`, `tblPointSlice`, and `tblUnitOfMeasure`, then reads selected numeric samples from `tblActualValueFloat` and `tblActualValueDigital`.

## Connection and permissions

Use a dedicated SQL-authenticated login with read-only access. Do not use a database owner, Windows administrator, or `sysadmin` login. Restrict the SQL Server firewall rule to the dashboard Mac.

Open **SQL Trends** on `http://127.0.0.1:3030` and configure:

- server hostname or IP and TCP port;
- historian database, normally `JCIHistorianDB`;
- SQL-authenticated read-only username and password;
- certificate validation policy;
- legacy TLS only when an obsolete server cannot negotiate modern TLS.

The password is stored in macOS Keychain. Non-secret settings are stored in the dashboard SQLite database. Settings changes and connection tests are accepted only from a browser on the host Mac.

## Graphing historian points

Open **Trend analysis** at `/trends` (administrator or operator account):

1. Filter by point family or equipment type, or search by controller, equipment, point name, or engineering unit.
2. Select up to eight points.
3. Choose a preset, including 3-year and 5-year views, or a custom range of up to 10 years.
4. Leave resolution on **Automatic**, or request a specific time bucket.
5. Select **Graph selected points**.

The catalog derives a readable equipment name and point family from each Metasys historian reference. When zone temperatures exist, the initial view filters to `ZN-T` across all equipment so the complete zone-temperature catalog is visible. Named terminal boxes sort first, and the equipment filter can narrow the results to terminal boxes, water-source heat pumps, other named equipment, or Metasys internal identifiers. Common HVAC families such as `ZN-T`, `SA-T`, `SA-F`, `SF-C`, `SF-S`, `DA-T`, `HWV-O`, and `HTG-O` are labeled and counted separately; other discovered suffixes remain searchable.

Queries use the indexed historian `PointSliceID` and UTC timestamp fields. Selected points are averaged into time buckets across the requested period, then displayed chronologically. The server automatically raises an overly fine requested resolution to keep responses within 5,000 samples.

The workspace provides independent scales for unlike engineering units, optional percent-change normalization, display-only moving-mean smoothing, drag-to-zoom, per-series minimum/maximum/average/change/linear-rate statistics, an accessible data table, and CSV export. Exports identify the actual bucket size and aggregation method and contain the unmodified samples returned by SQL.

The interaction model was informed by the open [FarmDashboard trend workspace](https://github.com/mdbro/farm_dashboard). The Metasys implementation is original, dependency-free browser code and does not copy FarmDashboard source.

## Advanced query

When no point is selected, the configured advanced query is used. It must be one read-only `SELECT` or `WITH` statement, use `@P1` and `@P2` for UTC start/end bounds, and return:

| Alias | SQL type | Meaning |
|---|---|---|
| `point_name` | text | Display name used to group graph series |
| `sample_time` | datetime/datetime2/datetimeoffset | Sample timestamp; plain datetime is treated as UTC |
| `sample_value` | numeric | Value plotted on the chart |
| `unit` | text or NULL | Engineering unit |

Statements containing writes, multiple statements, comments, external data access, administrative operations, or delay commands are rejected before reaching SQL Server.

## Legacy TLS

Legacy TLS is disabled by default. When enabled, SQL requests run in a separate helper process whose TLS policy permits TLS 1.0 and old cipher suites. The main dashboard, Metasys connector, and email transport retain their normal TLS policies. The SQL password is passed to the helper through a private process pipe, never through command-line arguments or environment variables.

Legacy TLS is a temporary compatibility measure. Patch or upgrade the SQL Server to TLS 1.2 or later, then disable the option.
