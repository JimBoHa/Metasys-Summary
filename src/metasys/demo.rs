use chrono::{Duration, Utc};

use crate::models::{AlarmRecord, FeedStatus, OverrideRecord, PointExceptionRecord, PollData};

pub fn poll_data() -> PollData {
    let equipment = [
        "AHU-1",
        "AHU-2",
        "Boiler-1",
        "CHW Plant",
        "RTU-3",
        "VAV-214",
        "Exhaust-2",
    ];
    let points = [
        "Supply Air Temp",
        "Static Pressure",
        "Discharge Temp",
        "Pump Status",
        "Zone Temperature",
        "Filter Differential",
    ];
    let alarm_types = [
        "alarmValueEnumSet.avHighLimit",
        "alarmValueEnumSet.avLowLimit",
        "alarmValueEnumSet.avOffline",
        "alarmValueEnumSet.avFault",
        "alarmValueEnumSet.avUnreliable",
    ];
    let priorities = [20_u16, 45, 70, 100, 130, 180, 220];
    let now = Utc::now();
    let mut alarms = Vec::new();
    for index in 0..186_usize {
        let day = (index * 7 + index / 4) % 30;
        let hour = (index * 11) % 24;
        let equipment_name = equipment[(index * index + index * 3) % equipment.len()];
        let point = points[(index * 5 + day) % points.len()];
        let alarm_type = alarm_types[(index + day) % alarm_types.len()];
        let priority = priorities[(index * 3 + day) % priorities.len()];
        let occurred_at = now - Duration::days(day as i64) - Duration::hours(hour as i64);
        let active = index < 14 && index % 4 != 0;
        alarms.push(AlarmRecord {
            id: format!("demo-alarm-{index:04}"),
            object_id: format!(
                "demo-point-{}-{}",
                index % equipment.len(),
                index % points.len()
            ),
            equipment: equipment_name.to_owned(),
            equipment_origin: "server".to_owned(),
            point: point.to_owned(),
            message: demo_message(alarm_type, equipment_name, point),
            alarm_type: alarm_type.to_owned(),
            category: if equipment_name.contains("Boiler") {
                "objectCategoryEnumSet.criticalEquipmentCategory".to_owned()
            } else {
                "objectCategoryEnumSet.hvacCategory".to_owned()
            },
            priority,
            occurred_at,
            active,
            acknowledged: index % 3 == 0,
            occurrence_count: 1,
            source: "demo".to_owned(),
            last_seen_at: Some(now),
        });
    }
    let active_alarms = alarms
        .iter()
        .filter(|alarm| alarm.active)
        .cloned()
        .collect();
    let overrides = vec![
        override_record("AHU-1", "Supply Air Setpoint", "58.0 °F", None),
        override_record("Boiler-1", "Enable Command", "Enabled", None),
        override_record(
            "VAV-214",
            "Damper Command",
            "100 %",
            Some(now + Duration::hours(2)),
        ),
        override_record("RTU-3", "Occupancy", "Occupied", None),
    ];
    let point_exceptions = overrides
        .iter()
        .map(|record| PointExceptionRecord {
            object_id: record.object_id.clone(),
            equipment: record.equipment.clone(),
            point: record.point.clone(),
            value: record.value.clone(),
            status: record.status.clone(),
            status_id: Some(86),
            kind: "override".to_owned(),
            expires_at: record.expires_at,
        })
        .collect::<Vec<_>>();
    PollData {
        connector: "Demo data".to_owned(),
        server_version: Some("Demo 1.0".to_owned()),
        alarms,
        active_alarms,
        overrides,
        exception_feed: FeedStatus::available(
            "Demonstration point-exception feed is available",
            point_exceptions.len(),
        ),
        point_exceptions,
    }
}

fn override_record(
    equipment: &str,
    point: &str,
    value: &str,
    expires_at: Option<chrono::DateTime<Utc>>,
) -> OverrideRecord {
    OverrideRecord {
        object_id: format!("demo-override-{}-{point}", equipment.to_ascii_lowercase()),
        equipment: equipment.to_owned(),
        point: point.to_owned(),
        value: value.to_owned(),
        status: "Operator Override".to_owned(),
        started_at: Some(Utc::now() - Duration::hours(3)),
        expires_at,
    }
}

fn demo_message(alarm_type: &str, equipment: &str, point: &str) -> String {
    let condition = if alarm_type.contains("Offline") {
        "communication offline"
    } else if alarm_type.contains("Fault") {
        "equipment fault"
    } else if alarm_type.contains("Low") {
        "below low limit"
    } else if alarm_type.contains("Unreliable") {
        "value unreliable"
    } else {
        "above high limit"
    };
    format!("{equipment} {point} {condition}")
}
