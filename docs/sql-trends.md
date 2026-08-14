# SQL trend setup

Metasys ADS/ADX installations store trend samples in Microsoft SQL Server, but raw repository schemas and names differ across Metasys releases and site designs. Expose only the required columns through a stable read-only view instead of granting this dashboard broad repository access.

## Result contract

The configured query must return:

| Alias | SQL type | Meaning |
|---|---|---|
| `point_name` | text | Unique, display-friendly point name |
| `sample_time` | datetime/datetime2/datetimeoffset | Sample timestamp; datetime values are treated as UTC |
| `sample_value` | numeric | Numeric sample value |
| `unit` | text or NULL | Engineering unit |

Example compatibility view after substituting the source tables and joins used by your Metasys release:

```sql
CREATE VIEW dbo.MetasysTrendSamples AS
SELECT
    CAST(source_point_name AS nvarchar(512)) AS point_name,
    source_timestamp AS sample_time,
    CAST(source_value AS float) AS sample_value,
    CAST(source_unit AS nvarchar(64)) AS unit
FROM dbo.YourMetasysTrendSource
```

The dashboard's default query is:

```sql
SELECT TOP (5000)
    CAST(point_name AS nvarchar(512)) AS point_name,
    sample_time,
    CAST(sample_value AS float) AS sample_value,
    CAST(unit AS nvarchar(64)) AS unit
FROM dbo.MetasysTrendSamples
WHERE sample_time >= @P1 AND sample_time <= @P2
ORDER BY sample_time ASC
```

Use a dedicated SQL-authenticated login that can connect only to the selected trend database and run `SELECT` on the compatibility view. Do not grant write, owner, system-administrator, or access to unrelated Metasys tables. Keep TCP access limited to the dashboard Mac at the firewall.

Open **SQL Trends** on `http://127.0.0.1:3030`, save the connection, and use **Test saved connection**. Passwords are sent only to the local dashboard process and stored in macOS Keychain.
