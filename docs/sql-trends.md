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

On the Operations page:

1. Select **Browse points**.
2. Search by controller, equipment, point name, or engineering unit.
3. Select up to eight points.
4. Choose a range from 24 hours through 5 years.
5. Select **Load trends**.

Queries use the indexed historian `PointSliceID` and UTC timestamp fields. Selected points are time-bucketed across the full requested period, then displayed chronologically. Responses are limited to 5,000 samples. No data is written to SQL Server.

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
