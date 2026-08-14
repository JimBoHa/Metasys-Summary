# Live hierarchy values

The equipment hierarchy at `/equipment` shows the newest Metasys SQL historian sample linked to each imported point. It is available to administrator and operator accounts and remains read-only.

## Refresh behavior

Each selected equipment view starts with a one-second refresh target. After every bounded SQL query, the server recommends the next start-to-start interval using both:

- point pressure: one second per 32 requested points, up to the 128-point endpoint limit;
- query pressure: four times the most recent SQL query duration, preserving headroom for other historian work.

The recommendation is clamped between one and 60 seconds. The browser honors it, prevents overlapping requests for the same selection, adds up to 10 percent positive jitter so multiple open dashboards do not synchronize, pauses while the tab is hidden, and doubles the interval after failures up to 60 seconds. A successful response restores the server-recommended rate.

The status beside the point count reports the active interval and why it was selected. Point rows continue to show the historian sample timestamp separately from the time the dashboard last checked for it; a fast dashboard refresh does not imply that the underlying controller or historian produces a new sample every second.

## MS/TP safety boundary

This page queries `JCIHistorianDB` with a bounded, read-only `SELECT`. It does not call the Metasys object API and does not send BACnet requests to an MS/TP trunk. The adaptive rate protects the SQL source and dashboard when an equipment item has many points or the query becomes slow; it cannot increase serial trunk traffic.

Any future direct BACnet live-data mode must use a separate, centrally budgeted collector with per-trunk request limits. It must not reuse the browser polling rate as an MS/TP polling rate.

## API response

`GET /api/equipment-values?pointSlices=...` includes the values plus refresh metadata:

- `refreshIntervalMilliseconds` is the precise recommendation used by the hierarchy;
- `refreshIntervalSeconds` is the rounded-up compatibility value;
- `refreshReason` is `oneSecondTarget`, `pointCount`, `queryLatency`, or `safetyCap`;
- `queryDurationMilliseconds` and `pointCount` explain the recommendation;
- `source` is `metasysSqlHistorian` and `pollsMstpTrunk` is always `false` for this endpoint.
