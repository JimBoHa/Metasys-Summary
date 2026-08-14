use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;
use chrono::{DateTime, Duration, TimeZone, Timelike, Utc};
use serde::Serialize;

use crate::{
    models::{
        AlarmRecord, FeedStatus, HealthView, OverrideRecord, PointExceptionRecord, clean_enum,
        infer_equipment_name, severity,
    },
    store::{PollRecord, Store},
};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsView {
    pub generated_at: DateTime<Utc>,
    pub health: HealthView,
    pub summary: DiagnosticSummary,
    pub findings: Vec<FindingView>,
    pub alarms: Vec<DiagnosticAlarmView>,
    pub equipment: Vec<EquipmentDiagnosticView>,
    pub systems: Vec<SystemDiagnosticView>,
    pub alarm_types: Vec<BreakdownView>,
    pub categories: Vec<BreakdownView>,
    pub sources: Vec<BreakdownView>,
    pub daily_activity: Vec<DailyActivityView>,
    pub hourly_activity_utc: Vec<HourlyActivityView>,
    pub overrides: Vec<OverrideRecord>,
    pub point_exceptions: Vec<PointExceptionRecord>,
    pub exception_feed: FeedStatus,
    pub poll_health: PollHealthView,
    pub data_quality: DataQualityView,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticSummary {
    pub active_alarm_count: usize,
    pub unacknowledged_active_count: usize,
    pub critical_active_count: usize,
    pub high_priority_active_count: usize,
    pub high_priority_unacknowledged_active_count: usize,
    pub fault_active_count: usize,
    pub offline_active_count: usize,
    pub stale_active_count: usize,
    pub override_count: usize,
    pub point_exception_count: usize,
    pub history_record_count: usize,
    pub history_occurrence_count: u64,
    pub equipment_count: usize,
    pub system_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingView {
    pub id: String,
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub recommendation: String,
    pub count: usize,
    pub tab: String,
    pub filter: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticAlarmView {
    pub id: String,
    pub object_id: String,
    pub system: String,
    pub equipment: String,
    pub equipment_origin: String,
    pub point: String,
    pub message: String,
    pub alarm_type: String,
    pub category: String,
    pub priority: u16,
    pub severity: String,
    pub occurred_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub active: bool,
    pub acknowledged: bool,
    pub occurrence_count: u64,
    pub source: String,
    pub stale: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EquipmentDiagnosticView {
    pub equipment: String,
    pub system: String,
    pub equipment_origin: String,
    pub point_count: usize,
    pub history_count: u64,
    pub active_count: usize,
    pub unacknowledged_count: usize,
    pub high_priority_count: usize,
    pub fault_count: usize,
    pub offline_count: usize,
    pub score: f64,
    pub last_alarm_at: DateTime<Utc>,
    pub top_condition: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemDiagnosticView {
    pub system: String,
    pub equipment_count: usize,
    pub point_count: usize,
    pub history_count: u64,
    pub active_count: usize,
    pub high_priority_count: usize,
    pub last_alarm_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakdownView {
    pub label: String,
    pub count: u64,
    pub percentage: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyActivityView {
    pub date: chrono::NaiveDate,
    pub total: u64,
    pub high_priority: u64,
    pub normal_returns: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HourlyActivityView {
    pub hour_utc: u32,
    pub count: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollHealthView {
    pub window_hours: i64,
    pub attempts: usize,
    pub successes: usize,
    pub failures: usize,
    pub success_percentage: f64,
    pub average_duration_ms: Option<u64>,
    pub maximum_duration_ms: Option<u64>,
    pub latest_attempt_at: Option<DateTime<Utc>>,
    pub latest_success_at: Option<DateTime<Utc>>,
    pub failures_detail: Vec<PollFailureView>,
    pub activity: Vec<PollActivityView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollFailureView {
    pub attempted_at: DateTime<Utc>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollActivityView {
    pub hour: DateTime<Utc>,
    pub attempts: usize,
    pub failures: usize,
    pub average_duration_ms: Option<u64>,
    pub maximum_active_alarms: usize,
    pub maximum_overrides: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DataQualityView {
    pub history_started_at: Option<DateTime<Utc>>,
    pub history_ended_at: Option<DateTime<Utc>>,
    pub total_records: usize,
    pub server_mapped_equipment: usize,
    pub inferred_equipment: usize,
    pub unknown_equipment: usize,
    pub equipment_mapping_percentage: f64,
    pub object_reference_percentage: f64,
    pub message_percentage: f64,
    pub category_percentage: f64,
    pub distinct_points: usize,
    pub distinct_equipment: usize,
    pub distinct_systems: usize,
    pub capabilities: Vec<CapabilityView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityView {
    pub name: String,
    pub state: String,
    pub detail: String,
}

pub fn build_diagnostics(
    store: &Store,
    health: HealthView,
    active_alarms: &[AlarmRecord],
    overrides: &[OverrideRecord],
    point_exceptions: &[PointExceptionRecord],
    exception_feed: FeedStatus,
    history_days: i64,
) -> Result<DiagnosticsView> {
    let now = Utc::now();
    let history = store.alarms_since(now - Duration::days(history_days.max(1)))?;
    let polls = store.polls_since(now - Duration::days(7))?;
    let mut available_records = history.clone();
    let mut available_ids = history
        .iter()
        .map(|alarm| alarm.id.clone())
        .collect::<HashSet<_>>();
    for alarm in active_alarms {
        if available_ids.insert(alarm.id.clone()) {
            available_records.push(alarm.clone());
        }
    }
    let active_ids = active_alarms
        .iter()
        .map(|alarm| alarm.id.as_str())
        .collect::<HashSet<_>>();
    let mut alarms = available_records
        .iter()
        .map(|alarm| alarm_view(alarm, active_ids.contains(alarm.id.as_str()), now))
        .collect::<Vec<_>>();
    alarms.sort_by_key(|alarm| {
        (
            !alarm.active,
            alarm.priority,
            std::cmp::Reverse(alarm.occurred_at),
        )
    });
    let equipment = equipment_diagnostics(&alarms);
    let systems = system_diagnostics(&alarms);
    let summary = summary(
        &alarms,
        &equipment,
        &systems,
        overrides,
        point_exceptions,
        history.len(),
        history.iter().map(|alarm| alarm.occurrence_count).sum(),
    );
    let poll_health = poll_health(&polls, now);
    let data_quality = data_quality(&available_records, &alarms, &health, &exception_feed);
    let findings = findings(
        &summary,
        &alarms,
        &equipment,
        &poll_health,
        &data_quality,
        &exception_feed,
    );

    Ok(DiagnosticsView {
        generated_at: now,
        health,
        summary,
        findings,
        alarm_types: breakdown(
            history.iter().map(|alarm| clean_enum(&alarm.alarm_type)),
            history.iter().map(|alarm| alarm.occurrence_count),
        ),
        categories: breakdown(
            history.iter().map(|alarm| clean_enum(&alarm.category)),
            history.iter().map(|alarm| alarm.occurrence_count),
        ),
        sources: breakdown(
            history.iter().map(|alarm| alarm.source.clone()),
            history.iter().map(|alarm| alarm.occurrence_count),
        ),
        daily_activity: daily_activity(&history, now),
        hourly_activity_utc: hourly_activity(&history),
        alarms,
        equipment,
        systems,
        overrides: overrides.to_vec(),
        point_exceptions: point_exceptions.to_vec(),
        exception_feed,
        poll_health,
        data_quality,
    })
}

fn alarm_view(alarm: &AlarmRecord, active: bool, now: DateTime<Utc>) -> DiagnosticAlarmView {
    let (equipment, equipment_origin) = effective_equipment(alarm);
    DiagnosticAlarmView {
        id: alarm.id.clone(),
        object_id: alarm.object_id.clone(),
        system: system_name(&alarm.object_id, &alarm.source),
        equipment,
        equipment_origin,
        point: alarm.point.clone(),
        message: alarm.message.clone(),
        alarm_type: clean_enum(&alarm.alarm_type),
        category: clean_enum(&alarm.category),
        priority: alarm.priority,
        severity: severity(alarm.priority).to_owned(),
        occurred_at: alarm.occurred_at,
        last_seen_at: alarm.last_seen_at,
        active,
        acknowledged: alarm.acknowledged,
        occurrence_count: alarm.occurrence_count,
        source: alarm.source.clone(),
        stale: active && alarm.occurred_at < now - Duration::hours(24),
    }
}

fn effective_equipment(alarm: &AlarmRecord) -> (String, String) {
    if !alarm.equipment.trim().is_empty() && alarm.equipment != "Unmapped equipment" {
        return (alarm.equipment.clone(), alarm.equipment_origin.clone());
    }
    infer_equipment_name(&alarm.object_id, &alarm.point).map_or_else(
        || ("Unmapped equipment".to_owned(), "unknown".to_owned()),
        |equipment| (equipment, "inferred".to_owned()),
    )
}

fn system_name(object_id: &str, source: &str) -> String {
    let candidate = object_id.split('/').next().unwrap_or(object_id).trim();
    if candidate.is_empty() {
        source.to_owned()
    } else {
        candidate.to_owned()
    }
}

#[derive(Default)]
struct EquipmentAggregate {
    system: String,
    origins: HashMap<String, usize>,
    points: HashSet<String>,
    history_count: u64,
    active_count: usize,
    unacknowledged_count: usize,
    high_priority_count: usize,
    fault_count: usize,
    offline_count: usize,
    latest: Option<DateTime<Utc>>,
    conditions: HashMap<String, u64>,
}

fn equipment_diagnostics(alarms: &[DiagnosticAlarmView]) -> Vec<EquipmentDiagnosticView> {
    let mut groups: HashMap<String, EquipmentAggregate> = HashMap::new();
    for alarm in alarms {
        let group = groups.entry(alarm.equipment.clone()).or_default();
        if group.system.is_empty() {
            group.system = alarm.system.clone();
        }
        *group
            .origins
            .entry(alarm.equipment_origin.clone())
            .or_default() += 1;
        group.points.insert(alarm.point.clone());
        group.history_count += alarm.occurrence_count.max(1);
        group.active_count += usize::from(alarm.active);
        group.unacknowledged_count += usize::from(alarm.active && !alarm.acknowledged);
        group.high_priority_count += usize::from(alarm.active && alarm.priority <= 79);
        group.fault_count += usize::from(alarm.active && is_fault(&alarm.alarm_type));
        group.offline_count += usize::from(alarm.active && is_offline(&alarm.alarm_type));
        *group
            .conditions
            .entry(alarm.alarm_type.clone())
            .or_default() += alarm.occurrence_count.max(1);
        if group.latest.is_none_or(|latest| alarm.occurred_at > latest) {
            group.latest = Some(alarm.occurred_at);
        }
    }
    let mut output = groups
        .into_iter()
        .map(|(equipment, group)| {
            let top_condition = group
                .conditions
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(label, _)| label)
                .unwrap_or_else(|| "No condition".to_owned());
            let equipment_origin = group
                .origins
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(origin, _)| origin)
                .unwrap_or_else(|| "unknown".to_owned());
            let score = group.history_count as f64
                + group.active_count as f64 * 8.0
                + group.high_priority_count as f64 * 12.0
                + group.fault_count as f64 * 7.0
                + group.offline_count as f64 * 10.0;
            EquipmentDiagnosticView {
                equipment,
                system: group.system,
                equipment_origin,
                point_count: group.points.len(),
                history_count: group.history_count,
                active_count: group.active_count,
                unacknowledged_count: group.unacknowledged_count,
                high_priority_count: group.high_priority_count,
                fault_count: group.fault_count,
                offline_count: group.offline_count,
                score: round1(score),
                last_alarm_at: group.latest.unwrap_or_else(Utc::now),
                top_condition,
            }
        })
        .collect::<Vec<_>>();
    output.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| right.active_count.cmp(&left.active_count))
            .then_with(|| left.equipment.cmp(&right.equipment))
    });
    output
}

#[derive(Default)]
struct SystemAggregate {
    equipment: HashSet<String>,
    points: HashSet<String>,
    history_count: u64,
    active_count: usize,
    high_priority_count: usize,
    latest: Option<DateTime<Utc>>,
}

fn system_diagnostics(alarms: &[DiagnosticAlarmView]) -> Vec<SystemDiagnosticView> {
    let mut groups: HashMap<String, SystemAggregate> = HashMap::new();
    for alarm in alarms {
        let group = groups.entry(alarm.system.clone()).or_default();
        group.equipment.insert(alarm.equipment.clone());
        group.points.insert(alarm.point.clone());
        group.history_count += alarm.occurrence_count.max(1);
        group.active_count += usize::from(alarm.active);
        group.high_priority_count += usize::from(alarm.active && alarm.priority <= 79);
        if group.latest.is_none_or(|latest| alarm.occurred_at > latest) {
            group.latest = Some(alarm.occurred_at);
        }
    }
    let mut output = groups
        .into_iter()
        .map(|(system, group)| SystemDiagnosticView {
            system,
            equipment_count: group.equipment.len(),
            point_count: group.points.len(),
            history_count: group.history_count,
            active_count: group.active_count,
            high_priority_count: group.high_priority_count,
            last_alarm_at: group.latest.unwrap_or_else(Utc::now),
        })
        .collect::<Vec<_>>();
    output.sort_by_key(|system| {
        (
            std::cmp::Reverse(system.high_priority_count),
            std::cmp::Reverse(system.active_count),
        )
    });
    output
}

fn summary(
    alarms: &[DiagnosticAlarmView],
    equipment: &[EquipmentDiagnosticView],
    systems: &[SystemDiagnosticView],
    overrides: &[OverrideRecord],
    point_exceptions: &[PointExceptionRecord],
    history_record_count: usize,
    history_occurrence_count: u64,
) -> DiagnosticSummary {
    let active = alarms
        .iter()
        .filter(|alarm| alarm.active)
        .collect::<Vec<_>>();
    DiagnosticSummary {
        active_alarm_count: active.len(),
        unacknowledged_active_count: active.iter().filter(|alarm| !alarm.acknowledged).count(),
        critical_active_count: active.iter().filter(|alarm| alarm.priority <= 39).count(),
        high_priority_active_count: active.iter().filter(|alarm| alarm.priority <= 79).count(),
        high_priority_unacknowledged_active_count: active
            .iter()
            .filter(|alarm| alarm.priority <= 79 && !alarm.acknowledged)
            .count(),
        fault_active_count: active
            .iter()
            .filter(|alarm| is_fault(&alarm.alarm_type))
            .count(),
        offline_active_count: active
            .iter()
            .filter(|alarm| is_offline(&alarm.alarm_type))
            .count(),
        stale_active_count: active.iter().filter(|alarm| alarm.stale).count(),
        override_count: overrides.len(),
        point_exception_count: point_exceptions.len(),
        history_record_count,
        history_occurrence_count,
        equipment_count: equipment.len(),
        system_count: systems.len(),
    }
}

fn findings(
    summary: &DiagnosticSummary,
    alarms: &[DiagnosticAlarmView],
    equipment: &[EquipmentDiagnosticView],
    poll_health: &PollHealthView,
    data_quality: &DataQualityView,
    exception_feed: &FeedStatus,
) -> Vec<FindingView> {
    let mut output = Vec::new();
    if summary.critical_active_count > 0 {
        output.push(finding(
            "critical-active",
            "critical",
            "Critical-priority alarms are active",
            format!(
                "{} active alarms have priorities 0–39.",
                summary.critical_active_count
            ),
            "Review these first, verify life-safety impact, then inspect the affected equipment and related trends.",
            summary.critical_active_count,
            ("alarms", "critical"),
        ));
    }
    let unacknowledged_high = alarms
        .iter()
        .filter(|alarm| alarm.active && !alarm.acknowledged && alarm.priority <= 79)
        .count();
    if unacknowledged_high > 0 {
        output.push(finding(
            "high-unacknowledged",
            "high",
            "High-priority alarms need review",
            format!("{unacknowledged_high} active high-priority alarms are not acknowledged."),
            "Confirm the conditions in Metasys, group related points by equipment, and document the response.",
            unacknowledged_high,
            ("alarms", "highUnacknowledged"),
        ));
    }
    if summary.offline_active_count > 0 {
        output.push(finding(
            "offline",
            "high",
            "Offline or communication conditions are active",
            format!("{} active alarms indicate offline communications.", summary.offline_active_count),
            "Check controller power/network reachability and look for multiple points sharing the same system path.",
            summary.offline_active_count,
            ("alarms", "offline"),
        ));
    }
    if summary.fault_active_count > 0 {
        output.push(finding(
            "fault-unreliable",
            "medium",
            "Fault or unreliable values are active",
            format!("{} active points report fault or unreliable status.", summary.fault_active_count),
            "Compare affected sensors with neighboring points and recent SQL trends before replacing hardware.",
            summary.fault_active_count,
            ("alarms", "fault"),
        ));
    }
    if summary.stale_active_count > 0 {
        output.push(finding(
            "stale-active",
            "medium",
            "Long-running alarms remain active",
            format!("{} active alarms were detected more than 24 hours ago.", summary.stale_active_count),
            "Separate known chronic conditions from new failures and verify that resolved events are returning to normal.",
            summary.stale_active_count,
            ("alarms", "stale"),
        ));
    }
    let noisy_equipment = equipment
        .iter()
        .filter(|item| item.history_count >= 10)
        .count();
    if noisy_equipment > 0 {
        output.push(finding(
            "repeat-equipment",
            "medium",
            "Equipment is generating repeated events",
            format!("{noisy_equipment} equipment groups produced ten or more events in the history window."),
            "Open the equipment ranking, identify repeated conditions, and compare the points on a common timeline.",
            noisy_equipment,
            ("equipment", "repeat"),
        ));
    }
    if poll_health.failures > 0 {
        output.push(finding(
            "poll-failures",
            "medium",
            "Metasys polling has intermittent failures",
            format!("{} of {} polls failed in the last seven days.", poll_health.failures, poll_health.attempts),
            "Review the failure timeline and duration data before trusting apparent gaps in alarms or overrides.",
            poll_health.failures,
            ("reliability", "failures"),
        ));
    }
    if exception_feed.state != "available" {
        output.push(finding(
            "exception-feed",
            "high",
            "Current point exceptions are unavailable",
            exception_feed.message.clone(),
            "Restore the read-only equipment-not-normal feed before treating a zero override count as authoritative.",
            1,
            ("reliability", "exceptionFeed"),
        ));
    }
    if data_quality.equipment_mapping_percentage < 80.0 {
        output.push(finding(
            "equipment-mapping",
            "info",
            "Equipment names are partly inferred",
            format!("Only {:.1}% of records include a server-provided equipment mapping; other names are inferred from point references.", data_quality.equipment_mapping_percentage),
            "Use system and point-reference drill-downs when confirming an inferred equipment grouping.",
            data_quality.inferred_equipment + data_quality.unknown_equipment,
            ("reliability", "mapping"),
        ));
    }
    output.sort_by_key(|item| finding_rank(&item.severity));
    output
}

fn finding(
    id: &str,
    severity: &str,
    title: &str,
    detail: String,
    recommendation: &str,
    count: usize,
    target: (&str, &str),
) -> FindingView {
    FindingView {
        id: id.to_owned(),
        severity: severity.to_owned(),
        title: title.to_owned(),
        detail,
        recommendation: recommendation.to_owned(),
        count,
        tab: target.0.to_owned(),
        filter: target.1.to_owned(),
    }
}

fn finding_rank(value: &str) -> u8 {
    match value {
        "critical" => 0,
        "high" => 1,
        "medium" => 2,
        _ => 3,
    }
}

fn poll_health(polls: &[PollRecord], now: DateTime<Utc>) -> PollHealthView {
    let attempts = polls.len();
    let successes = polls.iter().filter(|poll| poll.succeeded).count();
    let durations = polls
        .iter()
        .filter_map(|poll| poll.duration_ms)
        .collect::<Vec<_>>();
    let average_duration_ms =
        (!durations.is_empty()).then(|| durations.iter().sum::<u64>() / durations.len() as u64);
    let maximum_duration_ms = durations.iter().max().copied();
    let failures_detail = polls
        .iter()
        .filter(|poll| !poll.succeeded)
        .take(20)
        .map(|poll| PollFailureView {
            attempted_at: poll.attempted_at,
            message: poll
                .error_message
                .clone()
                .unwrap_or_else(|| "Poll failed without an error message".to_owned()),
        })
        .collect();
    PollHealthView {
        window_hours: 7 * 24,
        attempts,
        successes,
        failures: attempts.saturating_sub(successes),
        success_percentage: if attempts == 0 {
            0.0
        } else {
            round1(successes as f64 * 100.0 / attempts as f64)
        },
        average_duration_ms,
        maximum_duration_ms,
        latest_attempt_at: polls.first().map(|poll| poll.attempted_at),
        latest_success_at: polls
            .iter()
            .find(|poll| poll.succeeded)
            .map(|poll| poll.attempted_at),
        failures_detail,
        activity: poll_activity(polls, now),
    }
}

#[derive(Default)]
struct PollBucket {
    attempts: usize,
    failures: usize,
    duration_total: u64,
    duration_count: u64,
    maximum_active: usize,
    maximum_overrides: usize,
}

fn poll_activity(polls: &[PollRecord], now: DateTime<Utc>) -> Vec<PollActivityView> {
    let start = now - Duration::hours(23);
    let mut buckets = BTreeMap::new();
    for offset in 0..24 {
        let hour = (start + Duration::hours(offset)).timestamp() / 3_600;
        buckets.insert(hour, PollBucket::default());
    }
    for poll in polls.iter().filter(|poll| poll.attempted_at >= start) {
        let hour = poll.attempted_at.timestamp() / 3_600;
        if let Some(bucket) = buckets.get_mut(&hour) {
            bucket.attempts += 1;
            bucket.failures += usize::from(!poll.succeeded);
            if let Some(duration) = poll.duration_ms {
                bucket.duration_total += duration;
                bucket.duration_count += 1;
            }
            bucket.maximum_active = bucket.maximum_active.max(poll.active_alarm_count);
            bucket.maximum_overrides = bucket.maximum_overrides.max(poll.override_count);
        }
    }
    buckets
        .into_iter()
        .filter_map(|(hour, bucket)| {
            Some(PollActivityView {
                hour: Utc.timestamp_opt(hour * 3_600, 0).single()?,
                attempts: bucket.attempts,
                failures: bucket.failures,
                average_duration_ms: (bucket.duration_count > 0)
                    .then(|| bucket.duration_total / bucket.duration_count),
                maximum_active_alarms: bucket.maximum_active,
                maximum_overrides: bucket.maximum_overrides,
            })
        })
        .collect()
}

fn data_quality(
    history: &[AlarmRecord],
    alarms: &[DiagnosticAlarmView],
    health: &HealthView,
    exception_feed: &FeedStatus,
) -> DataQualityView {
    let total = history.len();
    let server_mapped_equipment = history
        .iter()
        .filter(|alarm| alarm.equipment_origin == "server")
        .count();
    let inferred_equipment = alarms
        .iter()
        .filter(|alarm| alarm.equipment_origin == "inferred")
        .count();
    let unknown_equipment = alarms
        .iter()
        .filter(|alarm| alarm.equipment_origin == "unknown")
        .count();
    let percentage = |count: usize| {
        if total == 0 {
            0.0
        } else {
            round1(count as f64 * 100.0 / total as f64)
        }
    };
    let distinct_points = alarms
        .iter()
        .map(|alarm| alarm.point.as_str())
        .collect::<HashSet<_>>()
        .len();
    let distinct_equipment = alarms
        .iter()
        .map(|alarm| alarm.equipment.as_str())
        .collect::<HashSet<_>>()
        .len();
    let distinct_systems = alarms
        .iter()
        .map(|alarm| alarm.system.as_str())
        .collect::<HashSet<_>>()
        .len();
    DataQualityView {
        history_started_at: history.iter().map(|alarm| alarm.occurred_at).min(),
        history_ended_at: history.iter().map(|alarm| alarm.occurred_at).max(),
        total_records: total,
        server_mapped_equipment,
        inferred_equipment,
        unknown_equipment,
        equipment_mapping_percentage: percentage(server_mapped_equipment),
        object_reference_percentage: percentage(
            history
                .iter()
                .filter(|alarm| !alarm.object_id.trim().is_empty())
                .count(),
        ),
        message_percentage: percentage(
            history
                .iter()
                .filter(|alarm| !alarm.message.trim().is_empty())
                .count(),
        ),
        category_percentage: percentage(
            history
                .iter()
                .filter(|alarm| !alarm.category.trim().is_empty())
                .count(),
        ),
        distinct_points,
        distinct_equipment,
        distinct_systems,
        capabilities: vec![
            CapabilityView {
                name: "Alarm history and active state".to_owned(),
                state: if health.state == "error" { "degraded" } else { "available" }.to_owned(),
                detail: format!("{} cached alarm records with current active and acknowledgement state", total),
            },
            CapabilityView {
                name: "Point references and priorities".to_owned(),
                state: if total > 0 { "available" } else { "waiting" }.to_owned(),
                detail: format!("{distinct_points} distinct point names across {distinct_systems} system paths"),
            },
            CapabilityView {
                name: "Equipment mappings".to_owned(),
                state: if percentage(server_mapped_equipment) >= 80.0 { "available" } else { "limited" }.to_owned(),
                detail: format!("{server_mapped_equipment} server mappings, {inferred_equipment} inferred, {unknown_equipment} unknown"),
            },
            CapabilityView {
                name: "Current point exceptions and overrides".to_owned(),
                state: exception_feed.state.clone(),
                detail: exception_feed.message.clone(),
            },
            CapabilityView {
                name: "Live mapped temperatures".to_owned(),
                state: "onDemand".to_owned(),
                detail: "Available for administrator-mapped floor-plan regions; values are read only when requested".to_owned(),
            },
            CapabilityView {
                name: "SQL historian trends".to_owned(),
                state: "separate".to_owned(),
                detail: "Available from the Trend Analysis page when the read-only SQL source is configured".to_owned(),
            },
        ],
    }
}

fn breakdown<L, C>(labels: L, counts: C) -> Vec<BreakdownView>
where
    L: Iterator<Item = String>,
    C: Iterator<Item = u64>,
{
    let mut groups: HashMap<String, u64> = HashMap::new();
    for (label, count) in labels.zip(counts) {
        *groups
            .entry(if label.trim().is_empty() {
                "Unknown".to_owned()
            } else {
                label
            })
            .or_default() += count.max(1);
    }
    let total = groups.values().sum::<u64>().max(1);
    let mut output = groups
        .into_iter()
        .map(|(label, count)| BreakdownView {
            label,
            count,
            percentage: round1(count as f64 * 100.0 / total as f64),
        })
        .collect::<Vec<_>>();
    output.sort_by_key(|item| std::cmp::Reverse(item.count));
    output
}

fn daily_activity(history: &[AlarmRecord], now: DateTime<Utc>) -> Vec<DailyActivityView> {
    let today = now.date_naive();
    let start = today - Duration::days(29);
    let mut days = BTreeMap::new();
    for offset in 0..30 {
        days.insert(start + Duration::days(offset), (0_u64, 0_u64, 0_u64));
    }
    for alarm in history {
        if let Some(day) = days.get_mut(&alarm.occurred_at.date_naive()) {
            let count = alarm.occurrence_count.max(1);
            day.0 += count;
            day.1 += count * u64::from(alarm.priority <= 79);
            day.2 += count * u64::from(is_normal(&alarm.alarm_type));
        }
    }
    days.into_iter()
        .map(
            |(date, (total, high_priority, normal_returns))| DailyActivityView {
                date,
                total,
                high_priority,
                normal_returns,
            },
        )
        .collect()
}

fn hourly_activity(history: &[AlarmRecord]) -> Vec<HourlyActivityView> {
    let mut hours = [0_u64; 24];
    for alarm in history {
        hours[alarm.occurred_at.hour() as usize] += alarm.occurrence_count.max(1);
    }
    hours
        .into_iter()
        .enumerate()
        .map(|(hour, count)| HourlyActivityView {
            hour_utc: hour as u32,
            count,
        })
        .collect()
}

fn is_fault(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("fault") || value.contains("unreliable")
}

fn is_offline(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("offline") || value.contains("communication")
}

fn is_normal(value: &str) -> bool {
    clean_enum(value).eq_ignore_ascii_case("normal")
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{Duration, Utc};
    use tempfile::tempdir;

    use super::{build_diagnostics, is_normal};
    use crate::{
        models::{AlarmRecord, FeedStatus, HealthView},
        store::Store,
    };

    fn alarm(
        id: &str,
        object_id: &str,
        point: &str,
        alarm_type: &str,
        priority: u16,
        active: bool,
    ) -> AlarmRecord {
        AlarmRecord {
            id: id.to_owned(),
            object_id: object_id.to_owned(),
            equipment: "Unmapped equipment".to_owned(),
            equipment_origin: "unknown".to_owned(),
            point: point.to_owned(),
            message: format!("{point} {alarm_type}"),
            alarm_type: alarm_type.to_owned(),
            category: "HVAC".to_owned(),
            priority,
            occurred_at: Utc::now() - Duration::hours(30),
            active,
            acknowledged: false,
            occurrence_count: 1,
            source: "test".to_owned(),
            last_seen_at: None,
        }
    }

    #[test]
    fn builds_actionable_diagnostics_and_infers_equipment() {
        let directory = tempdir().unwrap();
        let store = Arc::new(Store::open(&directory.path().join("diagnostics.sqlite3")).unwrap());
        let high = alarm(
            "high",
            "SERVER:NAE/AHU-4.SAT",
            "SAT",
            "High Alarm",
            45,
            true,
        );
        let mut old_active = alarm(
            "old-active",
            "SERVER:NAE/FC-1.TB6-P06",
            "TB6-P06",
            "Offline",
            120,
            true,
        );
        old_active.occurred_at = Utc::now() - Duration::days(45);
        let normal = alarm("normal", "SERVER:NAE/AHU-4.SAT", "SAT", "Normal", 45, false);
        store
            .upsert_alarms(&[high.clone(), old_active.clone(), normal])
            .unwrap();
        store.record_poll(true, 2, 0, 250, None).unwrap();

        let view = build_diagnostics(
            &store,
            HealthView::default(),
            &[high, old_active],
            &[],
            &[],
            FeedStatus::unavailable("test feed unavailable"),
            30,
        )
        .unwrap();

        assert_eq!(view.summary.active_alarm_count, 2);
        assert_eq!(view.summary.high_priority_active_count, 1);
        assert_eq!(view.summary.history_record_count, 2);
        assert!(
            view.alarms
                .iter()
                .any(|alarm| alarm.id == "old-active" && alarm.active)
        );
        assert_eq!(view.equipment[0].equipment, "AHU-4");
        assert_eq!(view.equipment[0].equipment_origin, "inferred");
        assert!(
            view.findings
                .iter()
                .any(|finding| finding.id == "high-unacknowledged")
        );
        assert!(
            view.findings
                .iter()
                .any(|finding| finding.id == "exception-feed")
        );
        assert_eq!(view.poll_health.successes, 1);
        assert_eq!(view.poll_health.average_duration_ms, Some(250));
    }

    #[test]
    fn normal_returns_do_not_include_not_normal_conditions() {
        assert!(is_normal("alarmValueEnumSet.avNormal"));
        assert!(!is_normal("Not Normal"));
    }
}
