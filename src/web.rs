use std::{convert::Infallible, sync::Arc};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;
use tower_http::trace::TraceLayer;

use crate::app::AppState;

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
        .fallback(not_found)
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

struct ApiError(anyhow::Error);

impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self(value)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        tracing::error!(error = %self.0, "dashboard API failed");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "dashboard data is temporarily unavailable"})),
        )
            .into_response()
    }
}

impl From<Infallible> for ApiError {
    fn from(value: Infallible) -> Self {
        match value {}
    }
}
