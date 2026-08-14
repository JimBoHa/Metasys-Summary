# System diagnostics

Administrators and operators can open `/diagnostics` from **Operations → System diagnostics**. The workspace is read-only. It analyzes the most recent in-memory Metasys poll plus the local rolling alarm and poll history; loading or filtering the page does not rerun upstream Metasys queries.

## Tabs

| Tab | Purpose |
|---|---|
| Triage | Prioritizes critical/high unacknowledged, offline, fault/unreliable, long-running, repeated-equipment, feed, and connection findings with a direct path to supporting records. |
| Alarm explorer | Searches and filters every cached alarm record by current/returned state, severity, acknowledgement, type, system path, message, equipment, point, source, or object reference. Selecting a row shows every collected field. Filtered rows can be exported as CSV. |
| Equipment & systems | Correlates alarms by equipment and by the leading Metasys object-reference path. Rankings show contributing points, current conditions, history volume, mapping origin, and a transparent diagnostic score. |
| Patterns | Charts 30-day event flow, high-priority events, normal returns, UTC hour-of-day concentration, alarm types, categories, and connector sources. |
| Overrides & exceptions | Shows operator overrides separately from the complete current equipment-not-normal feed, including point value, status text/identifier, expiry, and object reference. A failed feed is shown as unavailable rather than as a misleading zero. |
| Reliability & data | Shows seven-day poll success, new poll-duration measurements, hourly active-alarm/failure behavior, recent errors, field completeness, mapping quality, history coverage, and supported/limited feeds. |

## Equipment names and scores

Metasys records sometimes omit `MappedEquipments`. When that happens, the dashboard derives a useful equipment label from the point name and object reference. Every alarm and equipment group labels that name as **Server mapped**, **Inferred from reference**, or **Mapping unknown** so inferred groupings are never presented as authoritative server metadata.

The equipment diagnostic score is a sorting aid, not a Metasys severity value:

```text
history events
+ 8 × active alarms
+ 12 × active priority 0–79 alarms
+ 7 × active fault/unreliable alarms
+ 10 × active offline alarms
```

Always inspect the contributing alarm records and trends before deciding on corrective work.

## Feed behavior

The legacy connector carries the full authorization-data cookie into Alarm Manager, About, and Equipment Not Normal requests. Equipment Not Normal rows are retained as current point exceptions; override rows are the subset whose status identifier/text indicates an operator override. If that request fails, alarms remain available but diagnostics reports the exception feed as unavailable.

Modern REST connectors retain the alarm/activity and override data already supported by the selected API version. On-demand temperature reads remain associated with administrator-mapped floor-plan regions, and long-term point samples remain in the separate SQL-backed Trend Analysis workspace.
