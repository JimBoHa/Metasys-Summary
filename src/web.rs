use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::app::AppState;
use crate::sql_trends::SqlTrendSettingsUpdate;

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLES_CSS: &str = include_str!("../static/styles.css");

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(javascript))
        .route("/styles.css", get(stylesheet))
        .route("/api/dashboard", get(dashboard))
        .route("/api/health", get(health))
        .route("/api/refresh", post(refresh))
        .route(
            "/api/settings/sql",
            get(sql_settings).put(update_sql_settings),
        )
        .route("/api/settings/sql/test", post(test_sql_settings))
        .route("/api/trends", get(trends))
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn index() -> Response {
    static_response(INDEX_HTML, "text/html; charset=utf-8")
}

async fn javascript() -> Response {
    static_response(APP_JS, "text/javascript; charset=utf-8")
}

async fn stylesheet() -> Response {
    static_response(STYLES_CSS, "text/css; charset=utf-8")
}

async fn dashboard(
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::models::DashboardView>, ApiError> {
    state.dashboard().await.map(Json).map_err(ApiError::from)
}

async fn health(State(state): State<Arc<AppState>>) -> Json<crate::models::HealthView> {
    Json(state.health().await)
}

async fn refresh(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    tokio::spawn(async move {
        state.poll_once().await;
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "refresh scheduled"})),
    )
}

async fn sql_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<crate::sql_trends::SqlTrendSettingsView>, ApiError> {
    require_local(peer)?;
    state.sql_trend_settings().map(Json).map_err(ApiError::from)
}

async fn update_sql_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(update): Json<SqlTrendSettingsUpdate>,
) -> Result<Json<crate::sql_trends::SqlTrendSettingsView>, ApiError> {
    require_local(peer)?;
    state
        .update_sql_trend_settings(update)
        .map(Json)
        .map_err(ApiError::bad_request)
}

async fn test_sql_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_local(peer)?;
    state
        .test_sql_trend_connection()
        .await
        .map_err(ApiError::bad_gateway)?;
    Ok(Json(json!({"status": "connected"})))
}

#[derive(Deserialize)]
struct TrendQuery {
    hours: Option<i64>,
}

async fn trends(
    State(state): State<Arc<AppState>>,
    Query(query): Query<TrendQuery>,
) -> Result<Json<crate::sql_trends::TrendResponse>, ApiError> {
    state
        .sql_trends(query.hours.unwrap_or(24 * 7))
        .await
        .map(Json)
        .map_err(ApiError::bad_gateway)
}

fn require_local(peer: SocketAddr) -> Result<(), ApiError> {
    if peer.ip().is_loopback() {
        Ok(())
    } else {
        Err(ApiError {
            source: anyhow::anyhow!("settings request rejected from non-loopback address {peer}"),
            status: StatusCode::FORBIDDEN,
            public_message: "Settings can only be changed from a browser running on this Mac"
                .to_owned(),
        })
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
    (headers, content).into_response()
}

struct ApiError {
    source: anyhow::Error,
    status: StatusCode,
    public_message: String,
}

impl ApiError {
    fn bad_request(source: anyhow::Error) -> Self {
        let public_message = source.to_string();
        Self {
            source,
            status: StatusCode::BAD_REQUEST,
            public_message,
        }
    }

    fn bad_gateway(source: anyhow::Error) -> Self {
        let public_message = source.to_string();
        Self {
            source,
            status: StatusCode::BAD_GATEWAY,
            public_message,
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self {
            source: value,
            status: StatusCode::INTERNAL_SERVER_ERROR,
            public_message: "dashboard data is temporarily unavailable".to_owned(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.source, status = %self.status, "dashboard API failed");
        (self.status, Json(json!({"error": self.public_message}))).into_response()
    }
}

impl From<Infallible> for ApiError {
    fn from(value: Infallible) -> Self {
        match value {}
    }
}
