use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use chrono::{Duration, Utc};

use crate::{
    models::{
        AlarmRecord, AlarmView, DashboardView, EquipmentView, HealthView, OverrideRecord,
        SeriesPoint, SliceView, clean_enum,
    },
    store::Store,
};

pub fn build_dashboard(
    store: &Store,
    mut health: HealthView,
    active_alarms: &[AlarmRecord],
    overrides: &[OverrideRecord],
    history_days: i64,
) -> Result<DashboardView> {
    let now = Utc::now();
    let history = store.alarms_since(now - Duration::days(history_days))?;
    health.history_started_at = store.first_alarm_at()?;

    let mut active = active_alarms.to_vec();
    active.sort_by_key(|alarm| (alarm.priority, std::cmp::Reverse(alarm.occurred_at)));
    let active_views = active.iter().take(100).map(AlarmView::from).collect();

    let frequent = frequent_alarms(&history);
    let serious = serious_alarms(&history);
    let equipment = equipment_summary(&history, active_alarms);
    let alarm_rate = alarm_rate(&history, now);
    let by_type = pie_slices(
        history.iter().map(|alarm| clean_enum(&alarm.alarm_type)),
        history.iter().map(|alarm| alarm.occurrence_count),
        7,
    );
    let by_equipment = pie_slices(
        history.iter().map(|alarm| alarm.equipment.clone()),
        history.iter().map(|alarm| alarm.occurrence_count),
        6,
    );
    let thirty_day_alarm_count = history.iter().map(|alarm| alarm.occurrence_count).sum();
    let critical_active_count = active_alarms
        .iter()
        .filter(|alarm| alarm.priority <= 39)
        .count();

    Ok(DashboardView {
        generated_at: now,
        health,
        active_alarm_count: active_alarms.len(),
        critical_active_count,
        override_count: overrides.len(),
        thirty_day_alarm_count,
        active_alarms: active_views,
        frequent_alarms: frequent,
        serious_alarms: serious,
        overrides: overrides.iter().take(100).cloned().collect(),
        problematic_equipment: equipment,
        alarm_rate,
        alarms_by_type: by_type,
        alarms_by_equipment: by_equipment,
    })
}

fn frequent_alarms(history: &[AlarmRecord]) -> Vec<AlarmView> {
    let mut groups: HashMap<(String, String), (u64, &AlarmRecord)> = HashMap::new();
    for alarm in history {
        let key = (alarm.object_id.clone(), alarm.alarm_type.clone());
        let group = groups.entry(key).or_insert((0, alarm));
        group.0 += alarm.occurrence_count.max(1);
        if alarm.occurred_at > group.1.occurred_at {
            group.1 = alarm;
        }
    }
    let mut values = groups.into_values().collect::<Vec<_>>();
    values.sort_by_key(|(count, alarm)| (std::cmp::Reverse(*count), alarm.priority));
    values
        .into_iter()
        .take(15)
        .map(|(count, alarm)| {
            let mut view = AlarmView::from(alarm);
            view.count = Some(count);
            view
        })
        .collect()
}

fn serious_alarms(history: &[AlarmRecord]) -> Vec<AlarmView> {
    let mut alarms = history.to_vec();
    alarms.sort_by_key(|alarm| (alarm.priority, std::cmp::Reverse(alarm.occurred_at)));
    alarms.iter().take(15).map(AlarmView::from).collect()
}

fn equipment_summary(history: &[AlarmRecord], active: &[AlarmRecord]) -> Vec<EquipmentView> {
    #[derive(Default)]
    struct Aggregate {
        count: u64,
        active: u64,
        critical: u64,
        severity_points: f64,
        latest: Option<chrono::DateTime<Utc>>,
    }

    let mut groups: HashMap<String, Aggregate> = HashMap::new();
    for alarm in history {
        let group = groups.entry(alarm.equipment.clone()).or_default();
        group.count += alarm.occurrence_count.max(1);
        group.critical += u64::from(alarm.priority <= 39);
        group.severity_points += f64::from(256_u16.saturating_sub(alarm.priority)) / 64.0;
        if group.latest.is_none_or(|latest| alarm.occurred_at > latest) {
            group.latest = Some(alarm.occurred_at);
        }
    }
    for alarm in active {
        groups.entry(alarm.equipment.clone()).or_default().active += 1;
    }
    let total = groups.values().map(|group| group.count).sum::<u64>().max(1);
    let mut output = groups
        .into_iter()
        .map(|(equipment, group)| EquipmentView {
            equipment,
            alarm_count: group.count,
            active_count: group.active,
            critical_count: group.critical,
            score: round1(group.count as f64 + group.active as f64 * 3.0 + group.severity_points),
            percentage: round1(group.count as f64 * 100.0 / total as f64),
            last_alarm_at: group.latest.unwrap_or_else(Utc::now),
        })
        .collect::<Vec<_>>();
    output.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| right.alarm_count.cmp(&left.alarm_count))
    });
    output.truncate(15);
    output
}

fn alarm_rate(history: &[AlarmRecord], now: chrono::DateTime<Utc>) -> Vec<SeriesPoint> {
    let today = now.date_naive();
    let start = today - Duration::days(13);
    let mut counts = BTreeMap::new();
    for day_offset in 0..14 {
        counts.insert(start + Duration::days(day_offset), 0_u64);
    }
    for alarm in history {
        let day = alarm.occurred_at.date_naive();
        if let Some(count) = counts.get_mut(&day) {
            *count += 1;
        }
    }
    let entries = counts.into_iter().collect::<Vec<_>>();
    entries
        .iter()
        .enumerate()
        .map(|(index, (date, count))| {
            let window_start = index.saturating_sub(6);
            let window = &entries[window_start..=index];
            let average =
                window.iter().map(|(_, value)| *value).sum::<u64>() as f64 / window.len() as f64;
            SeriesPoint {
                date: *date,
                count: *count,
                rolling_average: round1(average),
            }
        })
        .collect()
}

fn pie_slices<L, C>(labels: L, counts: C, limit: usize) -> Vec<SliceView>
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
    let mut values = groups.into_iter().collect::<Vec<_>>();
    values.sort_by_key(|(_, count)| std::cmp::Reverse(*count));

    let mut slices = Vec::new();
    let mut other = 0_u64;
    for (index, (label, count)) in values.into_iter().enumerate() {
        if index < limit {
            slices.push(SliceView {
                label,
                count,
                percentage: round1(count as f64 * 100.0 / total as f64),
            });
        } else {
            other += count;
        }
    }
    if other > 0 {
        slices.push(SliceView {
            label: "Other".to_owned(),
            count: other,
            percentage: round1(other as f64 * 100.0 / total as f64),
        });
    }
    slices
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::{alarm_rate, pie_slices};

    #[test]
    fn alarm_rate_always_has_fourteen_days() {
        assert_eq!(alarm_rate(&[], Utc::now()).len(), 14);
    }

    #[test]
    fn pie_slices_roll_small_groups_into_other() {
        let labels = (0..5).map(|index| format!("Type {index}"));
        let values = std::iter::repeat_n(1, 5);
        let slices = pie_slices(labels, values, 2);
        assert_eq!(slices.len(), 3);
        assert_eq!(slices.last().unwrap().label, "Other");
        assert_eq!(slices.last().unwrap().count, 3);
    }

    #[test]
    fn chrono_duration_supports_date_math() {
        let today = Utc::now().date_naive();
        assert_eq!(today - Duration::days(13), today - Duration::days(13));
    }
}
