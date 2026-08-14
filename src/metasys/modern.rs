use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

use super::{
    AuthSession, ResolvedConnector, deduplicate_alarms, first_bool, first_string, first_u64,
    is_normal_alarm_type, parse_datetime, stable_id, value_to_string,
};
use crate::{
    config::Config,
    models::{AlarmRecord, OverrideRecord, PollData},
    portal::models::TemperatureReading,
};

pub async fn login(http: &Client, config: &Config) -> Result<AuthSession> {
    let password = config.password.as_deref().unwrap_or_default();
    let url = format!("{}/api/login", config.server_url);
    let response = http
        .post(&url)
        .json(&json!({"username": config.username, "password": password}))
        .send()
        .await
        .with_context(|| format!("connect to Metasys login at {url}"))?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("decode Metasys login response")?;
    if !status.is_success() {
        bail!("Metasys login failed ({status}): {}", error_message(&body));
    }
    let token = first_string(&body, &["/accessToken", "/access_token"])
        .context("Metasys login response did not contain an access token")?;
    let expires_at = parse_datetime(body.pointer("/expires"))
        .unwrap_or_else(|| Utc::now() + Duration::minutes(25));
    Ok(AuthSession {
        connector: ResolvedConnector::Modern {
            version: config.api_version.clone(),
        },
        token,
        expires_at,
        client_id: None,
        authorization_data: None,
    })
}

pub async fn detect_version(http: &Client, config: &Config, token: &str) -> Result<String> {
    for version in ["v6", "v5", "v4", "v3", "v2"] {
        let resource = if matches!(version, "v6" | "v5" | "v4") {
            "activities"
        } else {
            "alarms"
        };
        let url = format!("{}/api/{version}/{resource}", config.server_url);
        let response = http
            .get(&url)
            .bearer_auth(token)
            .query(&[("startTime", Utc::now().to_rfc3339())])
            .send()
            .await
            .with_context(|| format!("probe Metasys API {version}"))?;
        if response.status() != StatusCode::NOT_FOUND {
            return Ok(version.to_owned());
        }
    }
    bail!("Metasys REST login worked, but no supported API version (v2-v6) was found")
}

pub async fn fetch(http: &Client, config: &Config, token: &str, version: &str) -> Result<PollData> {
    let version_number = version
        .trim_start_matches(['v', 'V'])
        .parse::<u8>()
        .unwrap_or(4);
    let mut alarms = if version_number >= 5 {
        fetch_activities(http, config, token, version).await?
    } else {
        fetch_alarms(http, config, token, version).await?
    };
    alarms = deduplicate_alarms(alarms);
    alarms.sort_by_key(|alarm| std::cmp::Reverse(alarm.occurred_at));
    let active_alarms = alarms
        .iter()
        .filter(|alarm| alarm.active)
        .cloned()
        .collect();
    let overrides = fetch_overrides(http, config, token, version)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(error = %error, "Metasys override scan failed; alarm data remains available");
            Vec::new()
        });

    Ok(PollData {
        connector: format!("REST {version}"),
        server_version: Some(version.to_owned()),
        alarms,
        active_alarms,
        overrides,
    })
}

pub async fn read_temperature(
    http: &Client,
    config: &Config,
    token: &str,
    version: &str,
    object_id: &str,
    attribute_id: &str,
) -> Result<TemperatureReading> {
    let attribute = if attribute_id.trim().is_empty() || attribute_id.trim() == "85" {
        "presentValue"
    } else {
        attribute_id.trim()
    };
    let url = object_attribute_url(config, version, object_id, attribute)?;
    let response = http
        .get(url)
        .bearer_auth(token)
        .send()
        .await
        .context("read live Metasys temperature attribute")?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("decode live Metasys temperature attribute")?;
    if !status.is_success() {
        bail!("Metasys temperature read failed ({status})");
    }
    let mut reading = parse_temperature_attribute(&body, attribute)?;
    if reading.unit.is_empty()
        && let Ok(unit) = read_object_unit(http, config, token, version, object_id).await
    {
        reading.unit = unit;
    }
    Ok(reading)
}

fn object_attribute_url(
    config: &Config,
    version: &str,
    object_id: &str,
    attribute: &str,
) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&config.server_url).context("parse Metasys server URL")?;
    let mut segments = url
        .path_segments_mut()
        .map_err(|_| anyhow::anyhow!("Metasys server URL cannot contain this path"))?;
    segments.pop_if_empty().extend([
        "api",
        version,
        "objects",
        object_id.trim(),
        "attributes",
        attribute,
    ]);
    drop(segments);
    Ok(url)
}

async fn read_object_unit(
    http: &Client,
    config: &Config,
    token: &str,
    version: &str,
    object_id: &str,
) -> Result<String> {
    let response = http
        .get(object_attribute_url(config, version, object_id, "units")?)
        .bearer_auth(token)
        .send()
        .await
        .context("read live Metasys temperature units")?;
    if !response.status().is_success() {
        bail!("Metasys units attribute was unavailable");
    }
    let body = response
        .json::<Value>()
        .await
        .context("decode Metasys temperature units")?;
    let unit = first_string(
        &body,
        &[
            "/item/units",
            "/item/units/title",
            "/item/units/id",
            "/schema/properties/units/title",
        ],
    )
    .map(|value| display_unit(&value))
    .unwrap_or_default();
    if unit.is_empty() {
        bail!("Metasys units response did not contain a value");
    }
    Ok(unit)
}

fn display_unit(value: &str) -> String {
    let short = value.rsplit('.').next().unwrap_or(value).trim();
    match short.to_ascii_lowercase().as_str() {
        "degf" | "degreesfahrenheit" => "°F".to_owned(),
        "degc" | "degreescelsius" => "°C".to_owned(),
        "percent" | "percentrh" => "%".to_owned(),
        "kelvin" => "K".to_owned(),
        _ => short.to_owned(),
    }
}

fn parse_temperature_attribute(body: &Value, attribute: &str) -> Result<TemperatureReading> {
    let raw_value = body.get("item").and_then(|item| item.get(attribute));
    let value = raw_value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str()?.trim().parse::<f64>().ok())
    });
    let display_value = raw_value
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .or_else(|| value.map(|value| format!("{value:.1}")))
        .unwrap_or_default();
    if display_value.is_empty() {
        bail!("Metasys temperature response did not contain {attribute}");
    }
    let unit = body
        .pointer(&format!("/schema/properties/{attribute}/units/title"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let status_text = body
        .pointer(&format!("/condition/{attribute}/reliability"))
        .and_then(Value::as_str)
        .unwrap_or("Current")
        .rsplit('.')
        .next()
        .unwrap_or("Current")
        .to_owned();
    Ok(TemperatureReading {
        value,
        display_value,
        unit,
        status: status_text,
        observed_at: Utc::now(),
        available: true,
        error: None,
    })
}

async fn fetch_activities(
    http: &Client,
    config: &Config,
    token: &str,
    version: &str,
) -> Result<Vec<AlarmRecord>> {
    let url = format!("{}/api/{version}/activities", config.server_url);
    let start = Utc::now() - Duration::days(config.history_days);
    let query = vec![
        ("activityType", "alarm".to_owned()),
        ("startTime", start.to_rfc3339()),
        ("endTime", Utc::now().to_rfc3339()),
        ("includeDiscarded", "true".to_owned()),
        ("sort", "-creationTime".to_owned()),
        ("count", "500".to_owned()),
    ];
    fetch_pages(http, config, token, &url, &query, "modern-rest").await
}

async fn fetch_alarms(
    http: &Client,
    config: &Config,
    token: &str,
    version: &str,
) -> Result<Vec<AlarmRecord>> {
    let url = format!("{}/api/{version}/alarms", config.server_url);
    let start = Utc::now() - Duration::days(config.history_days);
    let mut query = vec![
        ("startTime", start.to_rfc3339()),
        ("endTime", Utc::now().to_rfc3339()),
        ("pageSize", "500".to_owned()),
        ("page", "1".to_owned()),
        ("sort", "-creationTime".to_owned()),
    ];
    let version_number = version
        .trim_start_matches(['v', 'V'])
        .parse::<u8>()
        .unwrap_or(4);
    if version_number <= 3 {
        query.extend([
            ("excludePending", "false".to_owned()),
            ("excludeAcknowledged", "false".to_owned()),
            ("excludeDiscarded", "false".to_owned()),
        ]);
    } else {
        query.extend([
            ("includeAcknowledged", "true".to_owned()),
            ("includeDiscarded", "true".to_owned()),
        ]);
    }
    fetch_pages(http, config, token, &url, &query, "modern-rest").await
}

async fn fetch_pages(
    http: &Client,
    config: &Config,
    token: &str,
    initial_url: &str,
    query: &[(impl AsRef<str>, String)],
    source: &str,
) -> Result<Vec<AlarmRecord>> {
    let mut next_url = Some(initial_url.to_owned());
    let mut first = true;
    let mut alarms = Vec::new();
    while let Some(url) = next_url.take() {
        let mut request = http.get(&url).bearer_auth(token);
        if first {
            let query_pairs = query
                .iter()
                .map(|(key, value)| (key.as_ref(), value.as_str()))
                .collect::<Vec<_>>();
            request = request.query(&query_pairs);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("request Metasys alarms from {url}"))?;
        let status = response.status();
        let body = response
            .json::<Value>()
            .await
            .with_context(|| format!("decode Metasys alarm response from {url}"))?;
        if !status.is_success() {
            bail!(
                "Metasys alarm request failed ({status}): {}",
                error_message(&body)
            );
        }
        if let Some(items) = body
            .get("items")
            .or_else(|| body.get("Items"))
            .and_then(Value::as_array)
        {
            alarms.extend(items.iter().map(|item| parse_alarm(item, source)));
        }
        if alarms.len() >= config.max_alarm_records {
            alarms.truncate(config.max_alarm_records);
            break;
        }
        next_url = first_string(&body, &["/next", "/Next"])
            .and_then(|candidate| normalize_next_url(&candidate, initial_url, &config.server_url));
        first = false;
    }
    Ok(alarms)
}

fn normalize_next_url(candidate: &str, initial_url: &str, server_url: &str) -> Option<String> {
    if candidate.starts_with(server_url) {
        return Some(candidate.to_owned());
    }
    if candidate.starts_with("/api/") {
        return Some(format!("{server_url}{candidate}"));
    }
    if candidate.starts_with('/') {
        let api_root = initial_url.rsplit_once('/')?.0;
        return Some(format!("{api_root}{candidate}"));
    }
    None
}

fn parse_alarm(value: &Value, source: &str) -> AlarmRecord {
    let object_id = first_string(value, &["/objectId", "/ObjectId"])
        .or_else(|| {
            first_string(value, &["/objectUrl", "/ObjectUrl"]).and_then(|url| {
                url.trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .map(str::to_owned)
            })
        })
        .unwrap_or_default();
    let occurred_at = parse_datetime(
        value
            .pointer("/creationTime")
            .or_else(|| value.pointer("/CreationTime")),
    )
    .unwrap_or_else(Utc::now);
    let point = first_string(value, &["/objectName", "/name", "/ObjectName", "/Name"])
        .unwrap_or_else(|| "Unknown point".to_owned());
    let alarm_type = first_string(
        value,
        &["/alarm/type", "/type", "/typeUrl", "/Alarm/Type", "/Type"],
    )
    .unwrap_or_else(|| "unknownAlarm".to_owned());
    let status = first_string(
        value,
        &["/activityManagementStatus", "/ActivityManagementStatus"],
    )
    .unwrap_or_else(|| "pending".to_owned());
    let discarded = status.eq_ignore_ascii_case("discarded")
        || first_bool(value, &["/isDiscarded", "/IsDiscarded"]).unwrap_or(false);
    let active = !discarded && !is_normal_alarm_type(&alarm_type);
    let equipment = first_string(
        value,
        &[
            "/equipment/0/equipmentName",
            "/equipment/0/name",
            "/equipment/0/shortName",
            "/Equipment/0/EquipmentName",
            "/Equipment/0/Name",
        ],
    )
    .unwrap_or_else(|| equipment_from_reference(value, &point));
    let message = first_string(
        value,
        &[
            "/alarm/message",
            "/message",
            "/alarm/description",
            "/description",
            "/Alarm/Message",
            "/Message",
            "/Description",
        ],
    )
    .unwrap_or_else(|| format!("{point} alarm"));
    let id = first_string(value, &["/id", "/alarm/id", "/Id", "/Alarm/Id"]).unwrap_or_else(|| {
        stable_id(&[&object_id, &alarm_type, &occurred_at.to_rfc3339(), &message])
    });

    AlarmRecord {
        id,
        object_id,
        equipment,
        point,
        message,
        alarm_type,
        category: first_string(
            value,
            &[
                "/alarm/category",
                "/category",
                "/categoryUrl",
                "/Alarm/Category",
                "/Category",
            ],
        )
        .unwrap_or_else(|| "generalCategory".to_owned()),
        priority: first_u64(
            value,
            &[
                "/alarm/priority",
                "/priority",
                "/Alarm/Priority",
                "/Priority",
            ],
        )
        .unwrap_or(255)
        .min(255) as u16,
        occurred_at,
        active,
        acknowledged: value
            .pointer("/alarm/acknowledgedTime")
            .is_some_and(|item| !item.is_null())
            || first_bool(value, &["/isAcknowledged", "/IsAcknowledged"]).unwrap_or(false),
        occurrence_count: 1,
        source: source.to_owned(),
    }
}

fn equipment_from_reference(value: &Value, point: &str) -> String {
    let reference = first_string(value, &["/itemReference"]).unwrap_or_default();
    let tail = reference.rsplit('/').next().unwrap_or(&reference);
    let candidate = tail
        .strip_suffix(point)
        .unwrap_or(tail)
        .trim_end_matches(['.', ':', '/'])
        .rsplit(['.', '/'])
        .next()
        .unwrap_or("");
    if candidate.is_empty() || candidate.starts_with('{') {
        "Unmapped equipment".to_owned()
    } else {
        candidate.to_owned()
    }
}

async fn fetch_overrides(
    http: &Client,
    config: &Config,
    token: &str,
    version: &str,
) -> Result<Vec<OverrideRecord>> {
    let url = format!("{}/api/{version}/objects", config.server_url);
    let response = http
        .get(&url)
        .bearer_auth(token)
        .query(&[("classification", "point"), ("flatten", "true")])
        .send()
        .await
        .context("list Metasys points for override scan")?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("decode Metasys point list")?;
    if !status.is_success() {
        bail!(
            "Metasys point list failed ({status}): {}",
            error_message(&body)
        );
    }
    let mut points = Vec::new();
    collect_object_metadata(&body, &mut points);
    points.sort_by(|left, right| left.id.cmp(&right.id));
    points.dedup_by(|left, right| left.id == right.id);
    points.truncate(config.max_override_points);

    let batch_url = format!("{}/api/{version}/objects/batch", config.server_url);
    let mut overridden = Vec::new();
    for chunk in points.chunks(100) {
        let requests = chunk
            .iter()
            .map(|point| {
                json!({
                    "id": point.id,
                    "relativeUrl": format!("{}/attributes/status", point.id)
                })
            })
            .collect::<Vec<_>>();
        let response = http
            .post(&batch_url)
            .bearer_auth(token)
            .json(&json!({"method": "GET", "requests": requests}))
            .send()
            .await
            .context("request Metasys point status batch")?;
        let status = response.status();
        let body = response
            .json::<Value>()
            .await
            .context("decode Metasys point status batch")?;
        if !status.is_success() {
            bail!(
                "Metasys override status batch failed ({status}): {}",
                error_message(&body)
            );
        }
        let point_map = chunk
            .iter()
            .map(|point| (point.id.as_str(), point))
            .collect::<HashMap<_, _>>();
        for item in body
            .get("responses")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = first_string(item, &["/id"]).unwrap_or_default();
            let status_text = first_string(item, &["/body/item/status"]).unwrap_or_default();
            if status_text
                .to_ascii_lowercase()
                .contains("operatoroverride")
                && let Some(point) = point_map.get(id.as_str())
            {
                overridden.push((*point).clone());
            }
        }
    }

    if overridden.is_empty() {
        return Ok(Vec::new());
    }
    fetch_override_details(http, config, token, version, &overridden).await
}

#[derive(Clone)]
struct PointMetadata {
    id: String,
    name: String,
    equipment: String,
}

fn collect_object_metadata(value: &Value, output: &mut Vec<PointMetadata>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_object_metadata(item, output);
            }
        }
        Value::Object(map) => {
            if let Some(id) = map.get("id").and_then(Value::as_str) {
                let name = map
                    .get("name")
                    .or_else(|| map.get("objectName"))
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown point")
                    .to_owned();
                let equipment = map
                    .get("itemReference")
                    .and_then(Value::as_str)
                    .map(|reference| {
                        reference
                            .rsplit(['.', '/'])
                            .nth(1)
                            .unwrap_or("Unmapped equipment")
                            .to_owned()
                    })
                    .unwrap_or_else(|| "Unmapped equipment".to_owned());
                output.push(PointMetadata {
                    id: id.to_owned(),
                    name,
                    equipment,
                });
            }
            for nested in map.values() {
                if nested.is_array() || nested.is_object() {
                    collect_object_metadata(nested, output);
                }
            }
        }
        _ => {}
    }
}

async fn fetch_override_details(
    http: &Client,
    config: &Config,
    token: &str,
    version: &str,
    points: &[PointMetadata],
) -> Result<Vec<OverrideRecord>> {
    let batch_url = format!("{}/api/{version}/objects/batch", config.server_url);
    let mut output = Vec::new();
    for chunk in points.chunks(50) {
        let mut requests = Vec::new();
        for point in chunk {
            requests.push(json!({"id": format!("{}:value", point.id), "relativeUrl": format!("{}/attributes/presentValue", point.id)}));
            requests.push(json!({"id": format!("{}:expires", point.id), "relativeUrl": format!("{}/attributes/overrideExpirationTime", point.id)}));
        }
        let response = http
            .post(&batch_url)
            .bearer_auth(token)
            .json(&json!({"method": "GET", "requests": requests}))
            .send()
            .await
            .context("request Metasys override details")?;
        let body = response
            .json::<Value>()
            .await
            .context("decode Metasys override details")?;
        let mut values: HashMap<String, String> = HashMap::new();
        let mut expirations: HashMap<String, Option<DateTime<Utc>>> = HashMap::new();
        for response in body
            .get("responses")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = first_string(response, &["/id"]).unwrap_or_default();
            if let Some((point_id, kind)) = id.rsplit_once(':') {
                if kind == "value" {
                    values.insert(
                        point_id.to_owned(),
                        value_to_string(response.pointer("/body/item/presentValue")),
                    );
                } else if kind == "expires" {
                    expirations.insert(
                        point_id.to_owned(),
                        parse_metasys_struct_datetime(
                            response.pointer("/body/item/overrideExpirationTime"),
                        ),
                    );
                }
            }
        }
        for point in chunk {
            output.push(OverrideRecord {
                object_id: point.id.clone(),
                equipment: point.equipment.clone(),
                point: point.name.clone(),
                value: values.get(&point.id).cloned().unwrap_or_default(),
                status: "Operator Override".to_owned(),
                started_at: None,
                expires_at: expirations.get(&point.id).copied().flatten(),
            });
        }
    }
    Ok(output)
}

fn parse_metasys_struct_datetime(value: Option<&Value>) -> Option<DateTime<Utc>> {
    if let Some(parsed) = super::parse_datetime(value) {
        return Some(parsed);
    }
    let value = value?;
    let date = value.get("date")?;
    let time = value.get("time")?;
    let year = first_u64(date, &["/year"])? as i32;
    let month = first_u64(date, &["/month"])? as u32;
    let day = first_u64(date, &["/dayOfMonth", "/day"])? as u32;
    let hour = first_u64(time, &["/hour"]).unwrap_or(0) as u32;
    let minute = first_u64(time, &["/minute"]).unwrap_or(0) as u32;
    let second = first_u64(time, &["/second"]).unwrap_or(0) as u32;
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let time = NaiveTime::from_hms_opt(hour, minute, second)?;
    Some(DateTime::from_naive_utc_and_offset(
        date.and_time(time),
        Utc,
    ))
}

fn error_message(value: &Value) -> String {
    first_string(
        value,
        &[
            "/error/message",
            "/error/statusName",
            "/message",
            "/title",
            "/Error/Message",
            "/Error/Type",
        ],
    )
    .unwrap_or_else(|| "unknown Metasys error".to_owned())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;

    use super::{display_unit, normalize_next_url, parse_alarm, parse_temperature_attribute};

    #[test]
    fn parses_v6_activity_alarm() {
        let value = json!({
            "id": "alarm-id",
            "objectId": "point-id",
            "objectName": "Zone Temp",
            "creationTime": "2026-08-01T12:00:00Z",
            "activityManagementStatus": "pending",
            "equipment": [{"equipmentName": "AHU-1"}],
            "alarm": {
                "message": "Temperature high",
                "type": "alarmValueEnumSet.avHiAlarm",
                "priority": 30,
                "category": "objectCategoryEnumSet.hvacCategory",
                "acknowledgedTime": null
            }
        });
        let alarm = parse_alarm(&value, "test");
        assert_eq!(alarm.equipment, "AHU-1");
        assert!(alarm.active);
        assert_eq!(alarm.priority, 30);
        assert_eq!(
            alarm.occurred_at,
            chrono::Utc.with_ymd_and_hms(2026, 8, 1, 12, 0, 0).unwrap()
        );
    }

    #[test]
    fn parses_v2_discarded_alarm() {
        let value = json!({
            "id": "old-alarm",
            "name": "Supply Fan",
            "itemReference": "SITE:AHU-2.SF-C",
            "creationTime": "2026-08-02T14:00:00Z",
            "message": "Fan failure",
            "typeUrl": "https://server/api/v2/enumSets/505/members/40",
            "priority": 25,
            "isAcknowledged": true,
            "isDiscarded": true,
            "categoryUrl": "https://server/api/v2/enumSets/33/members/5",
            "objectUrl": "https://server/api/v2/objects/point-id"
        });
        let alarm = parse_alarm(&value, "test");
        assert_eq!(alarm.object_id, "point-id");
        assert!(alarm.acknowledged);
        assert!(!alarm.active);
    }

    #[test]
    fn normalizes_safe_pagination_links() {
        let server = "https://metasys.local";
        let initial = "https://metasys.local/api/v3/alarms";
        assert_eq!(
            normalize_next_url("/alarms?page=2", initial, server).as_deref(),
            Some("https://metasys.local/api/v3/alarms?page=2")
        );
        assert_eq!(
            normalize_next_url("/api/v3/alarms?page=2", initial, server).as_deref(),
            Some("https://metasys.local/api/v3/alarms?page=2")
        );
        assert!(normalize_next_url("https://attacker.invalid/page", initial, server).is_none());
    }

    #[test]
    fn parses_rest_temperature_attribute() {
        let reading = parse_temperature_attribute(
            &json!({
                "item": {"presentValue": 71.8},
                "condition": {"presentValue": {"reliability": "reliabilityEnumSet.reliable"}}
            }),
            "presentValue",
        )
        .unwrap();
        assert_eq!(reading.value, Some(71.8));
        assert_eq!(reading.display_value, "71.8");
        assert_eq!(reading.unit, "");
        assert_eq!(reading.status, "reliable");
        assert_eq!(display_unit("unitEnumSet.degF"), "°F");
    }
}
