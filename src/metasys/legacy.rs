use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use reqwest::{Client, RequestBuilder, header};
use serde_json::{Value, json};

use super::{
    AuthSession, ResolvedConnector, deduplicate_alarms, first_bool, first_string, first_u64,
    is_normal_alarm_type, parse_datetime, stable_id, value_to_string,
};
use crate::{
    config::Config,
    models::{AlarmRecord, OverrideRecord, PollData},
};

pub async fn login(http: &Client, config: &Config) -> Result<AuthSession> {
    let password = config.password.as_deref().unwrap_or_default();
    let url = format!("{}/UI/api/Authentication/LogIn", config.server_url);
    let response = http
        .post(&url)
        .json(&json!({
            "username": config.username,
            "password": password,
            "domain": config.domain
        }))
        .send()
        .await
        .with_context(|| format!("connect to legacy Metasys UI login at {url}"))?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("decode legacy Metasys login response")?;
    if !status.is_success() || !body.get("Error").is_none_or(Value::is_null) {
        let error = first_string(
            &body,
            &["/Error/Message", "/Error/Type", "/Message", "/message"],
        )
        .unwrap_or_else(|| "unknown login error".to_owned());
        if error.eq_ignore_ascii_case("ChangePassword") {
            bail!(
                "Metasys account requires a password change. Sign into {}/UI/ as {} and complete password/terms setup, then run `metasys-dashboard configure`",
                config.server_url,
                config.username
            );
        }
        bail!("legacy Metasys login failed ({status}): {error}");
    }
    let token = first_string(&body, &["/Results/access_token"])
        .context("legacy Metasys login response did not contain an access token")?;
    if first_bool(&body, &["/Results/IsTermsAndConditionsAccepted"]) == Some(false) {
        bail!(
            "Metasys terms are not accepted for {}. Sign into {}/UI/ once, accept the terms, then retry",
            config.username,
            config.server_url
        );
    }
    Ok(AuthSession {
        connector: ResolvedConnector::Legacy,
        token,
        expires_at: Utc::now() + Duration::minutes(20),
    })
}

pub async fn fetch(http: &Client, config: &Config, token: &str) -> Result<PollData> {
    let current = fetch_alarm_set(http, config, token, None).await?;
    let history = fetch_alarm_set(
        http,
        config,
        token,
        Some(Utc::now() - Duration::days(config.history_days)),
    )
    .await
    .unwrap_or_else(|error| {
        tracing::warn!(error = %error, "legacy 30-day alarm query failed; using current alarms");
        current.clone()
    });
    let mut all = current.clone();
    all.extend(history);
    let alarms = deduplicate_alarms(all);
    let active_alarms = current
        .into_iter()
        .filter(|alarm| alarm.active)
        .collect::<Vec<_>>();
    let overrides = fetch_overrides(http, config, token)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(error = %error, "legacy override scan failed; alarm data remains available");
            Vec::new()
        });
    let server_version = fetch_version(http, config, token).await.ok();

    Ok(PollData {
        connector: "Legacy Metasys UI".to_owned(),
        server_version,
        alarms,
        active_alarms,
        overrides,
    })
}

async fn fetch_alarm_set(
    http: &Client,
    config: &Config,
    token: &str,
    start: Option<chrono::DateTime<Utc>>,
) -> Result<Vec<AlarmRecord>> {
    let url = format!("{}/UI/api/AlarmManagerService/GetAlarms", config.server_url);
    let date_range = start.map_or_else(
        || json!({"Minimum": "", "Maximum": ""}),
        |start| {
            json!({
                "Minimum": start.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                "Maximum": Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
            })
        },
    );
    let payload = json!({
        "DateRange": date_range,
        "PriorityRange": {"Minimum": "", "Maximum": ""},
        "Categories": [],
        "AlarmTypes": [],
        "IsAcknowledged": "",
        "Spaces": [],
        "Equipments": [],
        "SortColumn": "DetectionTime",
        "SortOrder": "DESC",
        "IsDesktop": true
    });
    let response = authorize(http.post(&url), token)
        .json(&payload)
        .send()
        .await
        .context("request alarms from legacy Metasys UI")?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("decode legacy Metasys alarm response")?;
    if !status.is_success() || !body.get("Error").is_none_or(Value::is_null) {
        bail!(
            "legacy alarm request failed ({status}): {}",
            legacy_error(&body)
        );
    }
    let items = body
        .pointer("/Results/AlarmResults")
        .and_then(Value::as_array)
        .context("legacy alarm response omitted Results.AlarmResults")?;
    Ok(items
        .iter()
        .take(config.max_alarm_records)
        .map(parse_alarm)
        .collect())
}

fn parse_alarm(value: &Value) -> AlarmRecord {
    let object_id = first_string(
        value,
        &["/PointReference", "/PointId", "/ObjectId", "/EquipmentId"],
    )
    .unwrap_or_default();
    let occurred_at = parse_datetime(
        value
            .get("DetectionTime")
            .or_else(|| value.get("TimeStampUTC"))
            .or_else(|| value.get("DateTime")),
    )
    .unwrap_or_else(Utc::now);
    let alarm_type =
        first_string(value, &["/AlarmType", "/Type"]).unwrap_or_else(|| "Unknown alarm".to_owned());
    let point = first_string(
        value,
        &["/ShortNames/0/Name", "/ShortNames/0", "/ItemName", "/Name"],
    )
    .unwrap_or_else(|| "Unknown point".to_owned());
    let equipment = first_string(
        value,
        &["/MappedEquipments/0/Name", "/EquipmentName", "/EquipmentId"],
    )
    .unwrap_or_else(|| "Unmapped equipment".to_owned());
    let message = first_string(
        value,
        &["/Message", "/Description", "/AlarmMessage", "/StatusText"],
    )
    .unwrap_or_else(|| format!("{point}: {alarm_type}"));
    let id = first_string(value, &["/Id", "/EventId", "/Guid"]).unwrap_or_else(|| {
        stable_id(&[&object_id, &alarm_type, &occurred_at.to_rfc3339(), &message])
    });
    let discarded = first_bool(value, &["/IsDiscarded"]).unwrap_or(false);

    AlarmRecord {
        id,
        object_id,
        equipment,
        point,
        message,
        alarm_type: alarm_type.clone(),
        category: first_string(value, &["/Category", "/CategoryName"])
            .unwrap_or_else(|| "General".to_owned()),
        priority: first_u64(value, &["/Priority"]).unwrap_or(255).min(255) as u16,
        occurred_at,
        active: !discarded && !is_normal_alarm_type(&alarm_type),
        acknowledged: first_bool(value, &["/IsAcknowledged"]).unwrap_or(false),
        occurrence_count: first_u64(
            value,
            &[
                "/OccurrenceCount",
                "/AlarmOccurrenceCount",
                "/AlarmOccurenceCount",
            ],
        )
        .unwrap_or(1)
        .max(1),
        source: "legacy-ui".to_owned(),
    }
}

async fn fetch_overrides(
    http: &Client,
    config: &Config,
    token: &str,
) -> Result<Vec<OverrideRecord>> {
    let url = format!("{}/UI/api/EquipmentNotNormal/GetPage", config.server_url);
    let mut snapshot_id = String::new();
    let mut offset = 0_usize;
    let page_size = 250_usize;
    let mut output = Vec::new();
    loop {
        let response = authorize(http.get(&url), token)
            .query(&[
                ("snapshotId", snapshot_id.as_str()),
                ("spaceId", "0"),
                ("offset", &offset.to_string()),
                ("count", &page_size.to_string()),
                ("sortColumn", "3"),
                ("reverse", "false"),
            ])
            .send()
            .await
            .context("request legacy Metasys problem areas")?;
        let status = response.status();
        let body = response
            .json::<Value>()
            .await
            .context("decode legacy Metasys problem areas")?;
        if !status.is_success() || !body.get("Error").is_none_or(Value::is_null) {
            bail!(
                "legacy override request failed ({status}): {}",
                legacy_error(&body)
            );
        }
        let results = body
            .get("Results")
            .context("legacy problem area response omitted Results")?;
        snapshot_id = first_string(results, &["/SnapshotId"]).unwrap_or_default();
        let total = first_u64(results, &["/TotalCount"]).unwrap_or(0) as usize;
        let problem_areas = results
            .get("ProblemAreas")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for item in &problem_areas {
            let status_text = first_string(item, &["/StatusText"]).unwrap_or_default();
            let status_id = first_u64(item, &["/StatusId"]).unwrap_or_default();
            if status_id == 86 || status_text.to_ascii_lowercase().contains("override") {
                output.push(OverrideRecord {
                    object_id: first_string(item, &["/PointId"]).unwrap_or_default(),
                    equipment: first_string(item, &["/EquipmentName"])
                        .unwrap_or_else(|| "Unmapped equipment".to_owned()),
                    point: first_string(item, &["/ShortName", "/Label"])
                        .unwrap_or_else(|| "Unknown point".to_owned()),
                    value: value_to_string(item.get("Value")),
                    status: if status_text.is_empty() {
                        "Operator Override".to_owned()
                    } else {
                        status_text
                    },
                    started_at: None,
                    expires_at: parse_datetime(item.get("StatusExpirationTime")),
                });
            }
        }
        offset += problem_areas.len();
        if problem_areas.is_empty() || offset >= total || offset >= config.max_override_points {
            break;
        }
    }
    Ok(output)
}

async fn fetch_version(http: &Client, config: &Config, token: &str) -> Result<String> {
    let url = format!("{}/UI/api/User/About", config.server_url);
    let response = authorize(http.get(url), token).send().await?;
    let body = response.json::<Value>().await?;
    let release = first_string(&body, &["/Results/ReleaseVersion"]);
    let software = first_string(&body, &["/Results/SoftwareVersion"]);
    match (release, software) {
        (Some(release), Some(software)) => Ok(format!("{release} ({software})")),
        (Some(value), None) | (None, Some(value)) => Ok(value),
        (None, None) => bail!("legacy version response omitted version fields"),
    }
}

fn legacy_error(value: &Value) -> String {
    first_string(
        value,
        &["/Error/Message", "/Error/Type", "/Message", "/message"],
    )
    .unwrap_or_else(|| "unknown legacy Metasys error".to_owned())
}

fn authorize(request: RequestBuilder, token: &str) -> RequestBuilder {
    request
        .bearer_auth(token)
        .header(header::COOKIE, format!("BearerToken={token}"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::parse_alarm;

    #[test]
    fn parses_legacy_alarm_manager_record() {
        let alarm = parse_alarm(&json!({
            "Id": "event-1",
            "PointReference": "SERVER:NAE/AHU-1.SAT",
            "ShortNames": ["SAT"],
            "MappedEquipments": [{"Name": "AHU-1"}],
            "AlarmType": "High Limit",
            "Priority": 35,
            "DetectionTime": "2026-08-01T12:00:00Z",
            "Message": "Supply temperature high",
            "IsAcknowledged": false
        }));
        assert_eq!(alarm.id, "event-1");
        assert_eq!(alarm.equipment, "AHU-1");
        assert_eq!(alarm.point, "SAT");
        assert!(alarm.active);
    }
}
