use std::{net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::{
    app::AppState,
    email_reports::EmailReportSettingsUpdate,
    portal::{
        models::{PortalRole, PortalSession},
        web::{PortalError, require_authenticated_role, require_request_csrf},
    },
    sql_trends::SqlTrendSettingsUpdate,
};

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLES_CSS: &str = include_str!("../static/styles.css");
const TRENDS_HTML: &str = include_str!("../static/trends.html");
const TRENDS_JS: &str = include_str!("../static/trends.js");
const TRENDS_CSS: &str = include_str!("../static/trends.css");

type WebResult<T> = Result<T, PortalError>;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/operations", get(index))
        .route("/trends", get(trends_index))
        .route("/app.js", get(javascript))
        .route("/styles.css", get(stylesheet))
        .route("/trends.js", get(trends_javascript))
        .route("/trends.css", get(trends_stylesheet))
        .route("/api/dashboard", get(dashboard))
        .route("/api/health", get(health))
        .route("/api/refresh", post(refresh))
        .route(
            "/api/settings/reports",
            get(report_settings).put(update_report_settings),
        )
        .route("/api/settings/reports/test", post(test_report_settings))
        .route("/api/reports/send", post(send_report_now))
        .route(
            "/api/settings/sql",
            get(sql_settings).put(update_sql_settings),
        )
        .route("/api/settings/sql/test", post(test_sql_settings))
        .route("/api/trend-points", get(trend_points))
        .route("/api/trends", get(trends))
        .merge(crate::portal::web::routes())
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn index(State(state): State<Arc<AppState>>, headers: HeaderMap) -> WebResult<Response> {
    require_role(&state, &headers, &[PortalRole::Admin, PortalRole::Operator])?;
    Ok(static_response(INDEX_HTML, "text/html; charset=utf-8"))
}

async fn trends_index(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> WebResult<Response> {
    require_role(&state, &headers, &[PortalRole::Admin, PortalRole::Operator])?;
    Ok(static_response(TRENDS_HTML, "text/html; charset=utf-8"))
}

async fn javascript() -> Response {
    static_response(APP_JS, "text/javascript; charset=utf-8")
}

async fn stylesheet() -> Response {
    static_response(STYLES_CSS, "text/css; charset=utf-8")
}

async fn trends_javascript() -> Response {
    static_response(TRENDS_JS, "text/javascript; charset=utf-8")
}

async fn trends_stylesheet() -> Response {
    static_response(TRENDS_CSS, "text/css; charset=utf-8")
}

async fn dashboard(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> WebResult<Json<crate::models::DashboardView>> {
    require_role(&state, &headers, &[PortalRole::Admin, PortalRole::Operator])?;
    state
        .dashboard()
        .await
        .map(Json)
        .map_err(PortalError::dashboard)
}

async fn health(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> WebResult<Json<crate::models::HealthView>> {
    require_role(&state, &headers, &[PortalRole::Admin, PortalRole::Operator])?;
    Ok(Json(state.health().await))
}

async fn refresh(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> WebResult<impl IntoResponse> {
    let session = require_role(&state, &headers, &[PortalRole::Admin, PortalRole::Operator])?;
    require_csrf(&headers, &session)?;
    tokio::spawn(async move {
        state.poll_once().await;
    });
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"status": "refresh scheduled"})),
    ))
}

async fn report_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> WebResult<Json<crate::email_reports::EmailReportSettingsView>> {
    require_role(&state, &headers, &[PortalRole::Admin])?;
    require_local_response(peer)?;
    state
        .email_report_settings()
        .map(Json)
        .map_err(PortalError::dashboard)
}

async fn update_report_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(update): Json<EmailReportSettingsUpdate>,
) -> WebResult<Json<crate::email_reports::EmailReportSettingsView>> {
    let session = require_role(&state, &headers, &[PortalRole::Admin])?;
    require_csrf(&headers, &session)?;
    require_local_response(peer)?;
    state
        .update_email_report_settings(update)
        .map(Json)
        .map_err(PortalError::bad_request)
}

async fn test_report_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> WebResult<Json<serde_json::Value>> {
    let session = require_role(&state, &headers, &[PortalRole::Admin])?;
    require_csrf(&headers, &session)?;
    require_local_response(peer)?;
    state
        .test_email_report_connection()
        .await
        .map_err(PortalError::bad_gateway)?;
    Ok(Json(json!({"status": "connected"})))
}

async fn send_report_now(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> WebResult<Json<crate::email_reports::EmailDeliveryResult>> {
    let session = require_role(&state, &headers, &[PortalRole::Admin])?;
    require_csrf(&headers, &session)?;
    require_local_response(peer)?;
    state
        .send_email_report_now()
        .await
        .map(Json)
        .map_err(PortalError::bad_gateway)
}

async fn sql_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> WebResult<Json<crate::sql_trends::SqlTrendSettingsView>> {
    require_role(&state, &headers, &[PortalRole::Admin])?;
    require_local_response(peer)?;
    state
        .sql_trend_settings()
        .map(Json)
        .map_err(PortalError::dashboard)
}

async fn update_sql_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(update): Json<SqlTrendSettingsUpdate>,
) -> WebResult<Json<crate::sql_trends::SqlTrendSettingsView>> {
    let session = require_role(&state, &headers, &[PortalRole::Admin])?;
    require_csrf(&headers, &session)?;
    require_local_response(peer)?;
    state
        .update_sql_trend_settings(update)
        .map(Json)
        .map_err(PortalError::bad_request)
}

async fn test_sql_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> WebResult<Json<serde_json::Value>> {
    let session = require_role(&state, &headers, &[PortalRole::Admin])?;
    require_csrf(&headers, &session)?;
    require_local_response(peer)?;
    state
        .test_sql_trend_connection()
        .await
        .map_err(PortalError::bad_gateway)?;
    Ok(Json(json!({"status": "connected"})))
}

#[derive(Deserialize)]
struct TrendQuery {
    hours: Option<i64>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    #[serde(rename = "intervalSeconds")]
    interval_seconds: Option<i64>,
    #[serde(default, rename = "pointSlices")]
    point_slices: Option<String>,
}

async fn trend_points(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> WebResult<Json<crate::sql_trends::TrendPointCatalog>> {
    require_role(&state, &headers, &[PortalRole::Admin, PortalRole::Operator])?;
    state
        .sql_trend_points()
        .await
        .map(Json)
        .map_err(PortalError::bad_gateway)
}

async fn trends(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<TrendQuery>,
) -> WebResult<Json<crate::sql_trends::TrendResponse>> {
    require_role(&state, &headers, &[PortalRole::Admin, PortalRole::Operator])?;
    let point_slice_ids = parse_point_slice_ids(query.point_slices.as_deref())?;
    let (from, to, interval_seconds) = resolve_trend_window(&query)?;
    state
        .sql_trends(from, to, interval_seconds, &point_slice_ids)
        .await
        .map(Json)
        .map_err(PortalError::bad_gateway)
}

fn resolve_trend_window(
    query: &TrendQuery,
) -> WebResult<(DateTime<Utc>, DateTime<Utc>, Option<i64>)> {
    if query.from.is_some() != query.to.is_some() {
        return Err(PortalError::bad_request(anyhow::anyhow!(
            "trend start and end must be supplied together"
        )));
    }
    if query.hours.is_some() && query.from.is_some() {
        return Err(PortalError::bad_request(anyhow::anyhow!(
            "use either a preset hour range or a custom start and end"
        )));
    }
    if query.interval_seconds.is_some_and(|seconds| seconds < 1) {
        return Err(PortalError::bad_request(anyhow::anyhow!(
            "trend interval must be a positive number of seconds"
        )));
    }
    let (from, to) = if let (Some(from), Some(to)) = (query.from, query.to) {
        (from, to)
    } else {
        let hours = query.hours.unwrap_or(24 * 7);
        if !(1..=24 * 365 * 10).contains(&hours) {
            return Err(PortalError::bad_request(anyhow::anyhow!(
                "trend hour range must be between 1 hour and 10 years"
            )));
        }
        let to = Utc::now();
        (to - Duration::hours(hours), to)
    };
    let range_seconds = (to - from).num_seconds();
    if !(1..=24 * 365 * 10 * 60 * 60).contains(&range_seconds) {
        return Err(PortalError::bad_request(anyhow::anyhow!(
            "trend start must precede the end and the range cannot exceed 10 years"
        )));
    }
    Ok((from, to, query.interval_seconds))
}

fn parse_point_slice_ids(value: Option<&str>) -> WebResult<Vec<i32>> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    value
        .split(',')
        .map(|part| {
            part.trim().parse::<i32>().map_err(|_| {
                PortalError::bad_request(anyhow::anyhow!(
                    "historian point selections must be numeric identifiers"
                ))
            })
        })
        .collect()
}

fn require_role(
    state: &AppState,
    headers: &HeaderMap,
    roles: &[PortalRole],
) -> WebResult<PortalSession> {
    require_authenticated_role(state, headers, roles)
}

fn require_csrf(headers: &HeaderMap, session: &PortalSession) -> WebResult<()> {
    require_request_csrf(headers, session)
}

fn require_local_response(peer: SocketAddr) -> WebResult<()> {
    require_local(peer)
}

fn require_local(peer: SocketAddr) -> Result<(), PortalError> {
    if peer.ip().is_loopback() {
        Ok(())
    } else {
        Err(PortalError::local_only(anyhow::anyhow!(
            "settings request rejected from non-loopback address {peer}"
        )))
    }
}

async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": "resource not found"})),
    )
}

fn static_response(content: &'static str, content_type: &'static str) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'",
        ),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
    );
    (headers, content).into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        net::{Ipv4Addr, SocketAddr},
        path::Path,
        sync::Arc,
    };

    use axum::{
        Router,
        body::{Body, to_bytes},
        extract::ConnectInfo,
        http::{HeaderValue, Method, Request, StatusCode, header},
        response::Response,
    };
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tower::ServiceExt;

    use super::{TrendQuery, resolve_trend_window, router};
    use crate::{
        app::AppState,
        config::{Config, ConnectorPreference},
        portal::{
            auth::hash_password,
            models::{BuildingInput, FloorInput, NormalizedPoint, PortalRole, RegionInput},
        },
        store::Store,
    };

    struct Login {
        cookie: String,
        csrf: String,
    }

    fn test_config(database_path: &Path) -> Config {
        Config {
            server_url: "https://metasys.example.invalid".to_owned(),
            username: "test-user".to_owned(),
            password: None,
            domain: "Metasys Local".to_owned(),
            connector: ConnectorPreference::Demo,
            api_version: "auto".to_owned(),
            bind_address: Ipv4Addr::LOCALHOST.into(),
            port: 3030,
            poll_interval_seconds: 60,
            history_days: 30,
            database_path: database_path.to_owned(),
            accept_invalid_certificates: false,
            open_browser: false,
            keychain_service: "portal-test".to_owned(),
            max_alarm_records: 1_000,
            max_override_points: 1_000,
        }
    }

    fn request(
        method: Method,
        path: &str,
        body: Option<Value>,
        login: Option<&Login>,
    ) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(path);
        builder = builder
            .header(header::HOST, "127.0.0.1:3030")
            .header("sec-fetch-site", "same-origin");
        if let Some(login) = login {
            builder = builder
                .header(header::COOKIE, &login.cookie)
                .header("x-csrf-token", &login.csrf);
        }
        let body = body.map_or_else(String::new, |value| value.to_string());
        if !body.is_empty() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let mut request = builder.body(Body::from(body)).unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((Ipv4Addr::LOCALHOST, 41000))));
        request
    }

    async fn call(app: &Router, request: Request<Body>) -> Response {
        app.clone().oneshot(request).await.unwrap()
    }

    async fn json_body(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[test]
    fn trend_query_validates_custom_windows_and_intervals() {
        let from = chrono::Utc::now() - chrono::Duration::days(2);
        let to = chrono::Utc::now();
        let resolved = resolve_trend_window(&TrendQuery {
            hours: None,
            from: Some(from),
            to: Some(to),
            interval_seconds: Some(300),
            point_slices: Some("1,2".to_owned()),
        })
        .ok()
        .unwrap();
        assert_eq!(resolved, (from, to, Some(300)));
        assert!(
            resolve_trend_window(&TrendQuery {
                hours: Some(24),
                from: Some(from),
                to: Some(to),
                interval_seconds: None,
                point_slices: None,
            })
            .is_err()
        );
        assert!(
            resolve_trend_window(&TrendQuery {
                hours: None,
                from: Some(from),
                to: None,
                interval_seconds: None,
                point_slices: None,
            })
            .is_err()
        );
        assert!(
            resolve_trend_window(&TrendQuery {
                hours: Some(24),
                from: None,
                to: None,
                interval_seconds: Some(0),
                point_slices: None,
            })
            .is_err()
        );
    }

    async fn login(app: &Router, email: &str) -> Login {
        let response = call(
            app,
            request(
                Method::POST,
                "/api/portal/login",
                Some(json!({"email": email, "password": "Portal-Test-47!"})),
                None,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let body = json_body(response).await;
        Login {
            cookie,
            csrf: body["csrfToken"].as_str().unwrap().to_owned(),
        }
    }

    #[tokio::test]
    async fn portal_bootstrap_is_local_and_single_use() {
        let directory = tempdir().unwrap();
        let database_path = directory.path().join("bootstrap.sqlite3");
        let store = Arc::new(Store::open(&database_path).unwrap());
        let state =
            Arc::new(AppState::new(Arc::new(test_config(&database_path)), store.clone()).unwrap());
        let app = router(state);

        let response = call(&app, request(Method::GET, "/api/portal/status", None, None)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let status = json_body(response).await;
        assert_eq!(status["initialized"], false);
        assert_eq!(status["bootstrapAllowed"], true);
        assert_eq!(status["localConfigurationAllowed"], true);
        assert_eq!(status["metasysConfigured"], false);

        let response = call(
            &app,
            request(Method::GET, "/api/portal/metasys-settings", None, None),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let settings = json_body(response).await;
        assert_eq!(settings["passwordConfigured"], false);
        assert_eq!(settings["serverUrl"], "https://metasys.example.invalid");

        let mut remote_settings = request(Method::GET, "/api/portal/metasys-settings", None, None);
        remote_settings
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((
                Ipv4Addr::new(192, 0, 2, 10),
                41000,
            ))));
        assert_eq!(
            call(&app, remote_settings).await.status(),
            StatusCode::FORBIDDEN
        );

        let invalid_connection = json!({
            "serverUrl": "https://metasys.example.invalid",
            "username": "browser-user",
            "password": "connection-password",
            "passwordConfirmation": "different-password",
            "domain": "Metasys Local",
            "connector": "auto",
            "apiVersion": "auto",
            "acceptInvalidCertificates": false
        });
        assert_eq!(
            call(
                &app,
                request(
                    Method::PUT,
                    "/api/portal/metasys-settings",
                    Some(invalid_connection.clone()),
                    None,
                ),
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );

        let body = json!({
            "displayName": "Initial Administrator",
            "email": "initial-admin@example.test",
            "password": "Portal-Bootstrap-47!",
            "passwordConfirmation": "Portal-Bootstrap-47!"
        });
        let mut lan_host = request(
            Method::POST,
            "/api/portal/bootstrap",
            Some(body.clone()),
            None,
        );
        lan_host.headers_mut().insert(
            header::HOST,
            HeaderValue::from_static("portal.example.test:3030"),
        );
        assert_eq!(call(&app, lan_host).await.status(), StatusCode::FORBIDDEN);

        let mut remote_peer = request(
            Method::POST,
            "/api/portal/bootstrap",
            Some(body.clone()),
            None,
        );
        remote_peer
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((
                Ipv4Addr::new(192, 0, 2, 10),
                41000,
            ))));
        assert_eq!(
            call(&app, remote_peer).await.status(),
            StatusCode::FORBIDDEN
        );

        let response = call(
            &app,
            request(
                Method::POST,
                "/api/portal/bootstrap",
                Some(body.clone()),
                None,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let session = json_body(response).await;
        assert_eq!(session["initialized"], true);
        assert_eq!(session["user"]["role"], "admin");
        assert_eq!(session["user"]["email"], "initial-admin@example.test");
        assert_eq!(store.portal_user_count().unwrap(), 1);

        let login = Login {
            cookie,
            csrf: session["csrfToken"].as_str().unwrap().to_owned(),
        };
        assert_eq!(
            call(
                &app,
                request(Method::GET, "/api/portal/me", None, Some(&login)),
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            call(
                &app,
                request(Method::GET, "/api/portal/metasys-settings", None, None,),
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(
                &app,
                request(
                    Method::GET,
                    "/api/portal/metasys-settings",
                    None,
                    Some(&login),
                ),
            )
            .await
            .status(),
            StatusCode::OK
        );
        let mut missing_csrf = request(
            Method::PUT,
            "/api/portal/metasys-settings",
            Some(invalid_connection),
            Some(&login),
        );
        missing_csrf.headers_mut().remove("x-csrf-token");
        assert_eq!(
            call(&app, missing_csrf).await.status(),
            StatusCode::FORBIDDEN
        );

        let response = call(&app, request(Method::GET, "/api/portal/status", None, None)).await;
        let status = json_body(response).await;
        assert_eq!(status["initialized"], true);
        assert_eq!(status["bootstrapAllowed"], false);
        assert_eq!(
            call(
                &app,
                request(Method::POST, "/api/portal/bootstrap", Some(body), None,),
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
    }

    #[tokio::test]
    async fn portal_enforces_roles_scopes_and_csrf() {
        let directory = tempdir().unwrap();
        let database_path = directory.path().join("portal.sqlite3");
        let store = Arc::new(Store::open(&database_path).unwrap());
        let building_a = store
            .create_building(&BuildingInput {
                name: "Building A".to_owned(),
                sort_order: 0,
            })
            .unwrap();
        let building_b = store
            .create_building(&BuildingInput {
                name: "Building B".to_owned(),
                sort_order: 1,
            })
            .unwrap();
        let floor_a = store
            .create_floor(&FloorInput {
                building_id: building_a,
                name: "First floor".to_owned(),
                sort_order: 0,
            })
            .unwrap();
        let floor_b = store
            .create_floor(&FloorInput {
                building_id: building_b,
                name: "First floor".to_owned(),
                sort_order: 0,
            })
            .unwrap();
        let polygon = vec![
            NormalizedPoint { x: 0.1, y: 0.1 },
            NormalizedPoint { x: 0.4, y: 0.1 },
            NormalizedPoint { x: 0.4, y: 0.4 },
        ];
        let create_region = |floor_id: &str, name: &str| {
            store
                .create_region(&RegionInput {
                    floor_id: floor_id.to_owned(),
                    name: name.to_owned(),
                    color: "#2cc7d2".to_owned(),
                    polygon: polygon.clone(),
                    fav_box: String::new(),
                    metasys_object_id: String::new(),
                    metasys_attribute_id: "85".to_owned(),
                })
                .unwrap()
        };
        let region_a = create_region(&floor_a, "North offices");
        let _region_a_other = create_region(&floor_a, "South offices");
        let _region_b = create_region(&floor_b, "Building B offices");

        let password_hash = hash_password("Portal-Test-47!").unwrap();
        store
            .create_portal_user(
                "admin@example.test",
                "Administrator",
                PortalRole::Admin,
                &password_hash,
                &[],
                &[],
            )
            .unwrap();
        store
            .create_portal_user(
                "viewer@example.test",
                "Viewer",
                PortalRole::ViewOnly,
                &password_hash,
                std::slice::from_ref(&floor_a),
                &[],
            )
            .unwrap();
        store
            .create_portal_user(
                "reporter@example.test",
                "Reporter",
                PortalRole::ReportingStaff,
                &password_hash,
                &[],
                std::slice::from_ref(&region_a),
            )
            .unwrap();
        store
            .create_portal_user(
                "operator@example.test",
                "Operator",
                PortalRole::Operator,
                &password_hash,
                &[],
                &[],
            )
            .unwrap();

        let state =
            Arc::new(AppState::new(Arc::new(test_config(&database_path)), store.clone()).unwrap());
        let app = router(state);

        let response = call(&app, request(Method::GET, "/api/portal/map", None, None)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let admin = login(&app, "admin@example.test").await;
        let mut missing_csrf = request(
            Method::POST,
            "/api/portal/admin/buildings",
            Some(json!({"name": "Building C"})),
            Some(&admin),
        );
        missing_csrf.headers_mut().remove("x-csrf-token");
        assert_eq!(
            call(&app, missing_csrf).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &app,
                request(
                    Method::POST,
                    "/api/portal/admin/buildings",
                    Some(json!({"name": "Building C"})),
                    Some(&admin),
                ),
            )
            .await
            .status(),
            StatusCode::OK
        );

        let viewer = login(&app, "viewer@example.test").await;
        assert_eq!(
            call(
                &app,
                request(
                    Method::GET,
                    "/api/portal/metasys-settings",
                    None,
                    Some(&viewer),
                ),
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        let response = call(
            &app,
            request(Method::GET, "/api/portal/map", None, Some(&viewer)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let map = json_body(response).await;
        assert_eq!(map["buildings"].as_array().unwrap().len(), 1);
        assert_eq!(map["buildings"][0]["name"], "Building A");
        assert_eq!(
            map["buildings"][0]["floors"][0]["regions"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            call(
                &app,
                request(Method::GET, "/operations", None, Some(&viewer)),
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&app, request(Method::GET, "/trends", None, Some(&viewer)),)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        let report_body = json!({
            "regionId": region_a,
            "contactEmail": "occupant@example.test",
            "issueType": "too_hot",
            "details": "Office feels warm"
        });
        assert_eq!(
            call(
                &app,
                request(
                    Method::POST,
                    "/api/portal/requests",
                    Some(report_body.clone()),
                    Some(&viewer),
                ),
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );

        let reporter = login(&app, "reporter@example.test").await;
        let response = call(
            &app,
            request(Method::GET, "/api/portal/map", None, Some(&reporter)),
        )
        .await;
        let map = json_body(response).await;
        assert_eq!(map["buildings"].as_array().unwrap().len(), 1);
        assert_eq!(
            map["buildings"][0]["floors"][0]["regions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let response = call(
            &app,
            request(
                Method::POST,
                "/api/portal/requests",
                Some(report_body.clone()),
                Some(&reporter),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let service_request = json_body(response).await;
        let request_id = service_request["id"].as_str().unwrap();
        assert_eq!(
            call(
                &app,
                request(
                    Method::POST,
                    &format!("/api/portal/requests/{request_id}/notes"),
                    Some(json!({"note": "Not allowed"})),
                    Some(&reporter),
                ),
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );

        let operator = login(&app, "operator@example.test").await;
        assert_eq!(
            call(
                &app,
                request(
                    Method::POST,
                    "/api/portal/requests",
                    Some(report_body),
                    Some(&operator),
                ),
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &app,
                request(
                    Method::POST,
                    &format!("/api/portal/requests/{request_id}/notes"),
                    Some(json!({"note": "Technician dispatched"})),
                    Some(&operator),
                ),
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            call(
                &app,
                request(Method::GET, "/operations", None, Some(&operator)),
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            call(&app, request(Method::GET, "/trends", None, Some(&operator)),)
                .await
                .status(),
            StatusCode::OK
        );
    }
}
