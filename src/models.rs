use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;

#[derive(Clone, Debug)]
pub struct AlarmRecord {
    pub id: String,
    pub object_id: String,
    pub equipment: String,
    pub point: String,
    pub message: String,
    pub alarm_type: String,
    pub category: String,
    pub priority: u16,
    pub occurred_at: DateTime<Utc>,
    pub active: bool,
    pub acknowledged: bool,
    pub occurrence_count: u64,
    pub source: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OverrideRecord {
    pub object_id: String,
    pub equipment: String,
    pub point: String,
    pub value: String,
    pub status: String,
    pub started_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct PollData {
    pub connector: String,
    pub server_version: Option<String>,
    pub alarms: Vec<AlarmRecord>,
    pub active_alarms: Vec<AlarmRecord>,
    pub overrides: Vec<OverrideRecord>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlarmView {
    pub id: String,
    pub equipment: String,
    pub point: String,
    pub message: String,
    pub alarm_type: String,
    pub category: String,
    pub priority: u16,
    pub severity: String,
    pub occurred_at: DateTime<Utc>,
    pub acknowledged: bool,
    pub count: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EquipmentView {
    pub equipment: String,
    pub alarm_count: u64,
    pub active_count: u64,
    pub critical_count: u64,
    pub score: f64,
    pub percentage: f64,
    pub last_alarm_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SeriesPoint {
    pub date: NaiveDate,
    pub count: u64,
    pub rolling_average: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SliceView {
    pub label: String,
    pub count: u64,
    pub percentage: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HealthView {
    pub state: String,
    pub message: String,
    pub connector: String,
    pub server_version: Option<String>,
    pub last_success_at: Option<DateTime<Utc>>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub history_started_at: Option<DateTime<Utc>>,
}

impl Default for HealthView {
    fn default() -> Self {
        Self {
            state: "starting".to_owned(),
            message: "Waiting for first Metasys poll".to_owned(),
            connector: "auto".to_owned(),
            server_version: None,
            last_success_at: None,
            last_attempt_at: None,
            history_started_at: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardView {
    pub generated_at: DateTime<Utc>,
    pub health: HealthView,
    pub active_alarm_count: usize,
    pub critical_active_count: usize,
    pub override_count: usize,
    pub thirty_day_alarm_count: u64,
    pub active_alarms: Vec<AlarmView>,
    pub frequent_alarms: Vec<AlarmView>,
    pub serious_alarms: Vec<AlarmView>,
    pub overrides: Vec<OverrideRecord>,
    pub problematic_equipment: Vec<EquipmentView>,
    pub alarm_rate: Vec<SeriesPoint>,
    pub alarms_by_type: Vec<SliceView>,
    pub alarms_by_equipment: Vec<SliceView>,
}

pub fn severity(priority: u16) -> &'static str {
    match priority {
        0..=39 => "critical",
        40..=79 => "high",
        80..=149 => "medium",
        _ => "low",
    }
}

pub fn clean_enum(value: &str) -> String {
    let tail = value.rsplit('.').next().unwrap_or(value);
    let tail = tail
        .strip_prefix("av")
        .filter(|rest| !rest.is_empty())
        .unwrap_or(tail);
    let mut output = String::new();
    for (index, character) in tail.chars().enumerate() {
        if index > 0 && character.is_ascii_uppercase() {
            output.push(' ');
        }
        output.push(character);
    }
    let mut characters = output.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().collect::<String>() + characters.as_str(),
        None => "Unknown".to_owned(),
    }
}

impl From<&AlarmRecord> for AlarmView {
    fn from(alarm: &AlarmRecord) -> Self {
        Self {
            id: alarm.id.clone(),
            equipment: alarm.equipment.clone(),
            point: alarm.point.clone(),
            message: alarm.message.clone(),
            alarm_type: clean_enum(&alarm.alarm_type),
            category: clean_enum(&alarm.category),
            priority: alarm.priority,
            severity: severity(alarm.priority).to_owned(),
            occurred_at: alarm.occurred_at,
            acknowledged: alarm.acknowledged,
            count: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{clean_enum, severity};

    #[test]
    fn cleans_metasys_enum_names() {
        assert_eq!(clean_enum("alarmValueEnumSet.avHighLimit"), "High Limit");
        assert_eq!(
            clean_enum("objectCategoryEnumSet.hvacCategory"),
            "Hvac Category"
        );
    }

    #[test]
    fn lower_priority_number_is_more_severe() {
        assert_eq!(severity(20), "critical");
        assert_eq!(severity(180), "low");
    }
}
