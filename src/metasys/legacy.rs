use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{Duration, Utc};
use reqwest::{Client, RequestBuilder, header};
use serde_json::{Value, json};

use super::{
    AuthSession, ResolvedConnector, deduplicate_alarms, first_bool, first_f64, first_string,
    first_u64, is_normal_alarm_type, parse_datetime, stable_id, value_to_string,
};
use crate::{
    config::Config,
    models::{AlarmRecord, OverrideRecord, PollData},
    portal::models::TemperatureReading,
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
    let client_id = first_string(&body, &["/Results/CurrentUserClientId"]);
    let authorization_data = body
        .get("Results")
        .map(serde_json::to_string)
        .transpose()
        .context("encode legacy Metasys authorization data")?;
    Ok(AuthSession {
        connector: ResolvedConnector::Legacy,
        token,
        expires_at: Utc::now() + Duration::minutes(20),
        client_id,
        authorization_data,
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

pub async fn read_temperature(
    http: &Client,
    config: &Config,
    session: &AuthSession,
    object_id: &str,
    attribute_id: &str,
    signalr: &mut Option<LegacySignalrConnection>,
) -> Result<TemperatureReading> {
    let token = &session.token;
    let authorization_data = session.authorization_data.as_deref();
    let client_id = session
        .client_id
        .as_deref()
        .context("legacy Metasys login response omitted CurrentUserClientId")?;
    let attribute = if attribute_id.eq_ignore_ascii_case("presentValue") {
        "85"
    } else {
        attribute_id.trim()
    };
    let reference = if object_id.contains(',') {
        object_id.trim().to_owned()
    } else {
        format!("{},{}", object_id.trim(), attribute)
    };

    if signalr
        .as_ref()
        .is_some_and(|connection| connection.created_at.elapsed() > StdDuration::from_secs(20))
        && let Some(connection) = signalr.take()
    {
        abort_signalr(
            http,
            config,
            token,
            authorization_data,
            client_id,
            &connection,
        )
        .await;
    }
    if signalr.is_none() {
        *signalr = Some(
            start_signalr(http, config, token, authorization_data, client_id)
                .await
                .context("start legacy Metasys live-data connection")?,
        );
    }

    let direct = read_point_value(http, config, token, authorization_data, &reference).await;
    if let Ok(reading) = direct {
        return Ok(reading);
    }
    let direct_error = direct.expect_err("successful direct reads return above");
    if !direct_error
        .to_string()
        .to_ascii_lowercase()
        .contains("sessionexpired")
    {
        return Err(direct_error);
    }

    let connection = signalr
        .as_mut()
        .context("legacy Metasys live-data connection was not initialized")?;
    match subscribe_temperature(
        http,
        config,
        token,
        authorization_data,
        client_id,
        connection,
        &reference,
    )
    .await
    {
        Ok(reading) => Ok(reading),
        Err(subscription_error) => {
            *signalr = None;
            bail!(
                "Metasys direct point read failed ({direct_error:#}); live-data subscription failed ({subscription_error:#})"
            )
        }
    }
}

const SIGNALR_CONNECTION_DATA: &str = r#"[{"name":"datavaluesservicehub"}]"#;

pub(super) struct LegacySignalrConnection {
    connection_token: String,
    connection_id: String,
    message_id: Option<String>,
    created_at: Instant,
}

async fn read_point_value(
    http: &Client,
    config: &Config,
    token: &str,
    authorization_data: Option<&str>,
    reference: &str,
) -> Result<TemperatureReading> {
    let url = format!("{}/UI/api/Point/GetPointValue", config.server_url);
    let response = authorize_with_data(
        http.get(&url)
            .query(&[("referenceAndAttributeId", &reference)]),
        token,
        authorization_data,
    )
    .send()
    .await
    .context("read live Metasys temperature point")?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .context("decode live Metasys temperature response")?;
    if !status.is_success() || !body.get("Error").is_none_or(Value::is_null) {
        let error_type = first_string(&body, &["/Error/Type"])
            .map(|value| format!(" [{value}]"))
            .unwrap_or_default();
        bail!(
            "Metasys temperature read failed ({status}): {}{error_type}",
            legacy_error(&body),
        );
    }
    parse_temperature(&body)
}

async fn start_signalr(
    http: &Client,
    config: &Config,
    token: &str,
    authorization_data: Option<&str>,
    client_id: &str,
) -> Result<LegacySignalrConnection> {
    let base = format!("{}/UI/signalr", config.server_url);
    let negotiate = signalr_base_request(
        http.get(format!("{base}/negotiate")),
        token,
        authorization_data,
        client_id,
    )
    .send()
    .await
    .context("negotiate legacy Metasys live-data connection")?;
    let negotiate = decode_signalr_response(negotiate, "negotiate")
        .await
        .context("legacy Metasys live-data negotiation failed")?;
    let connection_token = first_string(&negotiate, &["/ConnectionToken"])
        .context("SignalR negotiation omitted ConnectionToken")?;
    let connection_id = first_string(&negotiate, &["/ConnectionId"])
        .context("SignalR negotiation omitted ConnectionId")?;
    if first_string(&negotiate, &["/ProtocolVersion"]).as_deref() != Some("1.4") {
        bail!("legacy Metasys SignalR protocol is not compatible with client protocol 1.4");
    }

    let connect = signalr_connected_request(
        http.get(format!("{base}/connect")),
        token,
        authorization_data,
        client_id,
        &connection_token,
    )
    .timeout(StdDuration::from_secs(15))
    .send()
    .await
    .context("connect legacy Metasys live-data transport")?;
    let connect = decode_signalr_response(connect, "connect")
        .await
        .context("legacy Metasys live-data transport failed")?;
    if connect.get("S").and_then(Value::as_u64) != Some(1) {
        bail!("legacy Metasys live-data transport did not initialize");
    }

    let start = signalr_connected_request(
        http.get(format!("{base}/start")),
        token,
        authorization_data,
        client_id,
        &connection_token,
    )
    .timeout(StdDuration::from_secs(15))
    .send()
    .await
    .context("start legacy Metasys live-data transport")?;
    let start = decode_signalr_response(start, "start").await?;
    if first_string(&start, &["/Response"]).as_deref() != Some("started") {
        bail!("legacy Metasys live-data start response was not successful");
    }

    Ok(LegacySignalrConnection {
        connection_token,
        connection_id,
        message_id: first_string(&connect, &["/C"]),
        created_at: Instant::now(),
    })
}

async fn subscribe_temperature(
    http: &Client,
    config: &Config,
    token: &str,
    authorization_data: Option<&str>,
    client_id: &str,
    connection: &mut LegacySignalrConnection,
    reference: &str,
) -> Result<TemperatureReading> {
    let base = format!("{}/UI/signalr", config.server_url);
    let invocation = json!({
        "H": "datavaluesservicehub",
        "M": "subscribeDataValueUpdates",
        "A": [connection.connection_id, [reference]],
        "I": 0
    })
    .to_string();
    let send = signalr_connected_request(
        http.post(format!("{base}/send")),
        token,
        authorization_data,
        client_id,
        &connection.connection_token,
    )
    .form(&[("data", invocation)])
    .timeout(StdDuration::from_secs(15))
    .send()
    .await
    .context("subscribe to legacy Metasys point")?;
    let send = decode_signalr_response(send, "send").await?;
    if let Some(error) = first_string(&send, &["/E"]) {
        bail!("legacy Metasys point subscription was rejected: {error}");
    }
    if let Some(reading) = parse_signalr_temperature(&send, reference)? {
        return Ok(reading);
    }

    for _ in 0..2 {
        let mut request = signalr_connected_request(
            http.get(format!("{base}/poll")),
            token,
            authorization_data,
            client_id,
            &connection.connection_token,
        );
        if let Some(message_id) = connection.message_id.as_deref() {
            request = request.query(&[("messageId", message_id)]);
        }
        let response = match request.timeout(StdDuration::from_secs(8)).send().await {
            Ok(response) => response,
            Err(error) if error.is_timeout() => continue,
            Err(error) => return Err(error).context("poll legacy Metasys point value"),
        };
        let body = decode_signalr_response(response, "poll").await?;
        if let Some(message_id) = first_string(&body, &["/C"]) {
            connection.message_id = Some(message_id);
        }
        if let Some(reading) = parse_signalr_temperature(&body, reference)? {
            return Ok(reading);
        }
    }
    bail!("legacy Metasys live-data service did not return the mapped point")
}

fn parse_signalr_temperature(body: &Value, reference: &str) -> Result<Option<TemperatureReading>> {
    let Some(messages) = body.get("M").and_then(Value::as_array) else {
        return Ok(None);
    };
    for message in messages {
        if !first_string(message, &["/H"])
            .is_some_and(|hub| hub.eq_ignore_ascii_case("dataValuesServiceHub"))
            || !first_string(message, &["/M"])
                .is_some_and(|method| method.eq_ignore_ascii_case("processDataValuesUpdate"))
        {
            continue;
        }
        let Some(arguments) = message.get("A").and_then(Value::as_array) else {
            continue;
        };
        for encoded in arguments.iter().filter_map(Value::as_str) {
            let Ok(values) = serde_json::from_str::<Value>(encoded) else {
                continue;
            };
            let Some(values) = values.as_array() else {
                continue;
            };
            for value in values {
                if first_string(value, &["/Reference"])
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(reference))
                {
                    return parse_temperature(value).map(Some);
                }
            }
        }
    }
    Ok(None)
}

fn signalr_base_request(
    request: RequestBuilder,
    token: &str,
    authorization_data: Option<&str>,
    client_id: &str,
) -> RequestBuilder {
    authorize_with_data(request, token, authorization_data).query(&[
        ("clientProtocol", "1.4"),
        ("clientId", client_id),
        ("connectionData", SIGNALR_CONNECTION_DATA),
    ])
}

fn signalr_connected_request(
    request: RequestBuilder,
    token: &str,
    authorization_data: Option<&str>,
    client_id: &str,
    connection_token: &str,
) -> RequestBuilder {
    signalr_base_request(request, token, authorization_data, client_id).query(&[
        ("transport", "longPolling"),
        ("connectionToken", connection_token),
        ("tid", "0"),
    ])
}

async fn decode_signalr_response(response: reqwest::Response, operation: &str) -> Result<Value> {
    let status = response.status();
    let body = response
        .json::<Value>()
        .await
        .with_context(|| format!("decode legacy Metasys SignalR {operation} response"))?;
    if !status.is_success() {
        bail!("legacy Metasys SignalR {operation} failed ({status})");
    }
    Ok(body)
}

async fn abort_signalr(
    http: &Client,
    config: &Config,
    token: &str,
    authorization_data: Option<&str>,
    client_id: &str,
    connection: &LegacySignalrConnection,
) {
    let request = signalr_connected_request(
        http.post(format!("{}/UI/signalr/abort", config.server_url)),
        token,
        authorization_data,
        client_id,
        &connection.connection_token,
    )
    .timeout(StdDuration::from_secs(3));
    if let Err(error) = request.send().await {
        tracing::debug!(error = %error, "legacy Metasys live-data abort did not complete");
    }
}

fn parse_temperature(body: &Value) -> Result<TemperatureReading> {
    let display_value = first_string(
        body,
        &[
            "/Results/DataValue/ValueObject/Text",
            "/Results/ValueObject/Text",
            "/Results/DataValue/Text",
            "/Results/Value",
            "/DataValue/ValueObject/Text",
            "/ValueObject/Text",
            "/DataValue/Text",
            "/Value",
        ],
    )
    .unwrap_or_default();
    let value = first_f64(
        body,
        &[
            "/Results/DataValue/ValueObject/Value",
            "/Results/DataValue/ValueObject/Number",
            "/Results/ValueObject/Value",
            "/Results/ValueObject/Number",
            "/Results/Value",
            "/DataValue/ValueObject/Value",
            "/DataValue/ValueObject/Number",
            "/ValueObject/Value",
            "/ValueObject/Number",
            "/Value",
        ],
    )
    .or_else(|| display_value.parse::<f64>().ok());
    if display_value.is_empty() && value.is_none() {
        bail!("Metasys temperature response did not contain a point value");
    }
    let unit = first_string(
        body,
        &[
            "/Results/DataValue/ValueObject/Units",
            "/Results/ValueObject/Units",
            "/Results/Unit",
            "/DataValue/ValueObject/Units",
            "/ValueObject/Units",
            "/Unit",
        ],
    )
    .unwrap_or_default();
    let status = first_string(
        body,
        &[
            "/Results/DataValue/Status/Value/Text",
            "/Results/Status/Value/Text",
            "/Results/StatusText",
            "/DataValue/Status/Value/Text",
            "/Status/Value/Text",
            "/StatusText",
        ],
    )
    .unwrap_or_else(|| "Current".to_owned());
    Ok(TemperatureReading {
        value,
        display_value: if display_value.is_empty() {
            value.map(|value| format!("{value:.1}")).unwrap_or_default()
        } else {
            display_value
        },
        unit,
        status,
        observed_at: Utc::now(),
        available: true,
        error: None,
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
    authorize_with_data(request, token, None)
}

fn authorize_with_data(
    request: RequestBuilder,
    token: &str,
    authorization_data: Option<&str>,
) -> RequestBuilder {
    let cookie = authorization_data.map_or_else(
        || format!("BearerToken={token}"),
        |data| format!("BearerToken={token}; metasysAuthorizationData={data}"),
    );
    request
        .bearer_auth(token)
        .header(header::COOKIE, cookie)
        .header("X-Requested-With", "XMLHttpRequest")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_alarm, parse_signalr_temperature};

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

    #[test]
    fn parses_signalr_point_update() {
        let values = json!([{
            "Reference": "SERVER:DEVICE/FAV.ZN-T,85",
            "DataValue": {
                "ValueObject": {"Text": "72.4", "Units": "deg F"},
                "Status": {"Value": {"Text": "Normal"}}
            }
        }]);
        let envelope = json!({
            "C": "d-123",
            "M": [{
                "H": "dataValuesServiceHub",
                "M": "processDataValuesUpdate",
                "A": ["client-1", values.to_string()]
            }]
        });
        let reading = parse_signalr_temperature(&envelope, "SERVER:DEVICE/FAV.ZN-T,85")
            .unwrap()
            .unwrap();
        assert_eq!(reading.value, Some(72.4));
        assert_eq!(reading.display_value, "72.4");
        assert_eq!(reading.unit, "deg F");
        assert_eq!(reading.status, "Normal");
    }
}
