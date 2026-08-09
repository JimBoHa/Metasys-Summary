use std::{net::SocketAddr, sync::Arc};

use anyhow::{Result, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::json;

use crate::app::AppState;
use crate::config::MetasysConnectionUpdate;

use super::{
    auth::{
        SESSION_COOKIE, clean_upload_name, hash_password, random_token, token_hash,
        validate_building, validate_floor, validate_note, validate_plan_name, validate_region,
        validate_service_request, validate_trace, validate_user, verify_password,
    },
    floorplan::{MAX_PDF_BYTES, process_pdf},
    models::{
        AddNoteRequest, BootstrapRequest, BuildingInput, CreateServiceRequest, CreateUserRequest,
        FloorInput, LoginRequest, PortalRole, PortalSession, RegionInput, SessionView, TraceUpdate,
        UpdateRequestStatus, UpdateUserRequest,
    },
    store::SaveFloorPlan,
};

const PORTAL_HTML: &str = include_str!("../../static/portal.html");
const PORTAL_JS: &str = include_str!("../../static/portal.js");
const PORTAL_CSS: &str = include_str!("../../static/portal.css");
const UPLOAD_LIMIT: usize = MAX_PDF_BYTES + 1024 * 1024;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(portal_index))
        .route("/portal.js", get(portal_javascript))
        .route("/portal.css", get(portal_stylesheet))
        .route("/api/portal/status", get(portal_status))
        .route(
            "/api/portal/metasys-settings",
            get(metasys_settings).put(update_metasys_settings),
        )
        .route("/api/portal/bootstrap", post(bootstrap))
        .route("/api/portal/login", post(login))
        .route("/api/portal/logout", post(logout))
        .route("/api/portal/me", get(current_session))
        .route("/api/portal/map", get(portal_map))
        .route(
            "/api/portal/floorplans/{plan_id}/image",
            get(floor_plan_image),
        )
        .route("/api/portal/floorplans/{plan_id}/pdf", get(floor_plan_pdf))
        .route(
            "/api/portal/regions/{region_id}/temperature",
            get(region_temperature),
        )
        .route(
            "/api/portal/requests",
            get(list_requests).post(create_request),
        )
        .route(
            "/api/portal/requests/{request_id}/notes",
            post(add_request_note),
        )
        .route(
            "/api/portal/requests/{request_id}/status",
            axum::routing::put(update_request_status),
        )
        .route("/api/portal/admin/users", get(list_users).post(create_user))
        .route(
            "/api/portal/admin/users/{user_id}",
            axum::routing::put(update_user),
        )
        .route("/api/portal/admin/buildings", post(create_building))
        .route(
            "/api/portal/admin/buildings/{building_id}",
            axum::routing::put(update_building),
        )
        .route("/api/portal/admin/floors", post(create_floor))
        .route(
            "/api/portal/admin/floors/{floor_id}",
            axum::routing::put(update_floor),
        )
        .route(
            "/api/portal/admin/floorplans",
            post(upload_floor_plan).layer(DefaultBodyLimit::max(UPLOAD_LIMIT)),
        )
        .route(
            "/api/portal/admin/floorplans/{plan_id}/trace",
            axum::routing::put(update_floor_plan_trace),
        )
        .route("/api/portal/admin/regions", post(create_region))
        .route(
            "/api/portal/admin/regions/{region_id}",
            axum::routing::put(update_region).delete(delete_region),
        )
}

async fn portal_index() -> Response {
    static_response(PORTAL_HTML, "text/html; charset=utf-8")
}

async fn portal_javascript() -> Response {
    static_response(PORTAL_JS, "text/javascript; charset=utf-8")
}

async fn portal_stylesheet() -> Response {
    static_response(PORTAL_CSS, "text/css; charset=utf-8")
}

async fn portal_status(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, PortalError> {
    let initialized = state
        .store()
        .portal_user_count()
        .map_err(PortalError::internal)?
        > 0;
    let metasys_configured = state
        .metasys_connection_settings()
        .await
        .password_configured;
    let local_configuration_allowed = local_bootstrap_request(peer, &headers);
    Ok(Json(json!({
        "initialized": initialized,
        "bootstrapAllowed": !initialized && local_configuration_allowed,
        "localConfigurationAllowed": local_configuration_allowed,
        "metasysConfigured": metasys_configured,
    })))
}

async fn metasys_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<crate::config::MetasysConnectionView>, PortalError> {
    require_local_configuration_access(peer, &state, &headers)?;
    Ok(Json(state.metasys_connection_settings().await))
}

async fn update_metasys_settings(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<MetasysConnectionUpdate>,
) -> Result<Json<crate::config::MetasysConnectionResult>, PortalError> {
    require_same_site(&headers)?;
    let session = require_local_configuration_access(peer, &state, &headers)?;
    if let Some(session) = session.as_ref() {
        require_request_csrf(&headers, session)?;
    }
    input.validated().map_err(PortalError::bad_request)?;
    state
        .update_metasys_connection(input)
        .await
        .map(Json)
        .map_err(PortalError::bad_gateway)
}

async fn bootstrap(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<BootstrapRequest>,
) -> Result<Response, PortalError> {
    require_same_site(&headers)?;
    if !local_bootstrap_request(peer, &headers) {
        return Err(PortalError::new(
            StatusCode::FORBIDDEN,
            "Initial setup must be completed at http://127.0.0.1:3030 on the host Mac",
        ));
    }
    if state
        .store()
        .portal_user_count()
        .map_err(PortalError::internal)?
        > 0
    {
        return Err(PortalError::new(
            StatusCode::CONFLICT,
            "The maintenance portal is already initialized",
        ));
    }
    validate_user(&input.email, &input.display_name).map_err(PortalError::bad_request)?;
    if input.password != input.password_confirmation {
        return Err(PortalError::bad_request(anyhow!(
            "password confirmation does not match"
        )));
    }
    let password = input.password;
    let password_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|error| PortalError::internal(anyhow!("password task failed: {error}")))?
        .map_err(PortalError::bad_request)?;
    let token = random_token();
    let csrf_token = random_token();
    let peer_ip = peer.ip().to_string();
    let user = state
        .store()
        .create_initial_portal_admin(
            &input.email,
            &input.display_name,
            &password_hash,
            &token_hash(&token),
            &csrf_token,
            &peer_ip,
        )
        .map_err(PortalError::internal)?
        .ok_or_else(|| {
            PortalError::new(
                StatusCode::CONFLICT,
                "The maintenance portal is already initialized",
            )
        })?;
    let mut response = Json(SessionView {
        user,
        csrf_token,
        initialized: true,
    })
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie(&token, request_is_secure(&headers)))
            .map_err(PortalError::internal)?,
    );
    Ok(response)
}

async fn login(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<LoginRequest>,
) -> Result<Response, PortalError> {
    require_same_site(&headers)?;
    if input.email.len() > 254 || input.password.len() > 256 {
        return Err(PortalError::unauthorized("Email or password is incorrect"));
    }
    let peer_ip = peer.ip().to_string();
    if state
        .store()
        .login_is_rate_limited(&input.email, &peer_ip)
        .map_err(PortalError::internal)?
    {
        return Err(PortalError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many login attempts. Wait 15 minutes and retry.",
        ));
    }
    let user = state
        .store()
        .portal_user_for_login(&input.email)
        .map_err(PortalError::internal)?;
    let password = input.password;
    let verification_hash = user
        .as_ref()
        .map(|user| user.password_hash.clone())
        .unwrap_or_else(|| {
            hash_password("Missing-Portal-Account-47")
                .expect("fixed dummy portal password must satisfy policy")
        });
    let verified =
        tokio::task::spawn_blocking(move || verify_password(&password, &verification_hash))
            .await
            .map_err(|error| PortalError::internal(anyhow!("password task failed: {error}")))?;
    let authenticated = verified && user.as_ref().is_some_and(|user| user.active);
    state
        .store()
        .record_login_attempt(&input.email, &peer_ip, authenticated)
        .map_err(PortalError::internal)?;
    if !authenticated {
        return Err(PortalError::unauthorized("Email or password is incorrect"));
    }
    let user = user.expect("authenticated login has a user");
    let token = random_token();
    let csrf_token = random_token();
    state
        .store()
        .create_portal_session(&user.id, &token_hash(&token), &csrf_token, &peer_ip)
        .map_err(PortalError::internal)?;
    let view = state
        .store()
        .portal_user_view(&user.id)
        .map_err(PortalError::internal)?;
    let response = Json(SessionView {
        user: view,
        csrf_token,
        initialized: true,
    });
    let mut response = response.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie(&token, request_is_secure(&headers)))
            .map_err(PortalError::internal)?,
    );
    Ok(response)
}

async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PortalError> {
    let session = require_session(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    state
        .store()
        .delete_portal_session(&session.token_hash)
        .map_err(PortalError::internal)?;
    let mut response = Json(json!({"status": "signedOut"})).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "metasys_portal_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        ),
    );
    Ok(response)
}

async fn current_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<SessionView>, PortalError> {
    let session = require_session(&state, &headers)?;
    let user = state
        .store()
        .portal_user_view(&session.user.id)
        .map_err(PortalError::internal)?;
    Ok(Json(SessionView {
        user,
        csrf_token: session.csrf_token,
        initialized: true,
    }))
}

async fn portal_map(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<super::models::PortalMapView>, PortalError> {
    let session = require_session(&state, &headers)?;
    state
        .portal_map(&session.user)
        .await
        .map(Json)
        .map_err(PortalError::internal)
}

async fn floor_plan_image(
    Path(plan_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PortalError> {
    let session = require_session(&state, &headers)?;
    let record = state
        .store()
        .floor_plan_data_for_user(&plan_id, &session.user)
        .map_err(PortalError::forbidden_from)?;
    binary_response(record.image_data, "image/png", None)
}

async fn floor_plan_pdf(
    Path(plan_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Response, PortalError> {
    let session = require_session(&state, &headers)?;
    let record = state
        .store()
        .floor_plan_data_for_user(&plan_id, &session.user)
        .map_err(PortalError::forbidden_from)?;
    binary_response(
        record.pdf_data,
        "application/pdf",
        Some(&record.view.source_file_name),
    )
}

async fn region_temperature(
    Path(region_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<super::models::TemperatureReading>, PortalError> {
    let session = require_session(&state, &headers)?;
    if !state
        .store()
        .user_can_access_region(&session.user, &region_id)
        .map_err(PortalError::internal)?
    {
        return Err(PortalError::forbidden());
    }
    let region = state
        .store()
        .region_record(&region_id)
        .map_err(PortalError::internal)?
        .ok_or_else(PortalError::not_found)?;
    Ok(Json(state.temperature_for_region(&region).await))
}

async fn list_requests(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<super::models::ServiceRequestView>>, PortalError> {
    let session = require_session(&state, &headers)?;
    state
        .store()
        .list_service_requests(&session.user)
        .map(Json)
        .map_err(PortalError::internal)
}

async fn create_request(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<CreateServiceRequest>,
) -> Result<Json<super::models::ServiceRequestView>, PortalError> {
    let session = require_session(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    if !session.user.role.can_report() {
        return Err(PortalError::forbidden());
    }
    validate_service_request(&input).map_err(PortalError::bad_request)?;
    if !state
        .store()
        .user_can_access_region(&session.user, &input.region_id)
        .map_err(PortalError::internal)?
    {
        return Err(PortalError::forbidden());
    }
    state
        .store()
        .create_service_request(&session.user.id, &input)
        .map(Json)
        .map_err(PortalError::bad_request)
}

async fn add_request_note(
    Path(request_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<AddNoteRequest>,
) -> Result<Json<super::models::ServiceRequestView>, PortalError> {
    let session = require_session(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    if !session.user.role.can_note() {
        return Err(PortalError::forbidden());
    }
    validate_note(&input.note).map_err(PortalError::bad_request)?;
    state
        .store()
        .add_service_request_note(&request_id, &session.user.id, &input.note)
        .map(Json)
        .map_err(PortalError::bad_request)
}

async fn update_request_status(
    Path(request_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<UpdateRequestStatus>,
) -> Result<Json<super::models::ServiceRequestView>, PortalError> {
    let session = require_session(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    if !session.user.role.can_note() {
        return Err(PortalError::forbidden());
    }
    state
        .store()
        .update_service_request_status(&request_id, input.status)
        .map(Json)
        .map_err(PortalError::bad_request)
}

async fn list_users(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Vec<super::models::PortalUserView>>, PortalError> {
    let session = require_admin(&state, &headers)?;
    let _ = session;
    state
        .store()
        .list_portal_users()
        .map(Json)
        .map_err(PortalError::internal)
}

async fn create_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<CreateUserRequest>,
) -> Result<Json<super::models::PortalUserView>, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    validate_user(&input.email, &input.display_name).map_err(PortalError::bad_request)?;
    let password = input.password.clone();
    let password_hash = tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|error| PortalError::internal(anyhow!("password task failed: {error}")))?
        .map_err(PortalError::bad_request)?;
    state
        .store()
        .create_portal_user(
            &input.email,
            &input.display_name,
            input.role,
            &password_hash,
            &input.floor_ids,
            &input.region_ids,
        )
        .map(Json)
        .map_err(PortalError::bad_request)
}

async fn update_user(
    Path(user_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<UpdateUserRequest>,
) -> Result<Json<super::models::PortalUserView>, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    validate_user(&input.email, &input.display_name).map_err(PortalError::bad_request)?;
    if user_id == session.user.id && (!input.active || input.role != PortalRole::Admin) {
        return Err(PortalError::bad_request(anyhow!(
            "an administrator cannot deactivate or demote their own account"
        )));
    }
    let password_hash = if let Some(password) = input.password.clone()
        && !password.is_empty()
    {
        Some(
            tokio::task::spawn_blocking(move || hash_password(&password))
                .await
                .map_err(|error| PortalError::internal(anyhow!("password task failed: {error}")))?
                .map_err(PortalError::bad_request)?,
        )
    } else {
        None
    };
    state
        .store()
        .update_portal_user(&user_id, &input, password_hash.as_deref())
        .map(Json)
        .map_err(PortalError::bad_request)
}

async fn create_building(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<BuildingInput>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    validate_building(&input).map_err(PortalError::bad_request)?;
    let id = state
        .store()
        .create_building(&input)
        .map_err(PortalError::bad_request)?;
    Ok(Json(json!({"id": id})))
}

async fn update_building(
    Path(building_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<BuildingInput>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    validate_building(&input).map_err(PortalError::bad_request)?;
    state
        .store()
        .update_building(&building_id, &input)
        .map_err(PortalError::bad_request)?;
    Ok(Json(json!({"id": building_id})))
}

async fn create_floor(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<FloorInput>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    validate_floor(&input).map_err(PortalError::bad_request)?;
    let id = state
        .store()
        .create_floor(&input)
        .map_err(PortalError::bad_request)?;
    Ok(Json(json!({"id": id})))
}

async fn update_floor(
    Path(floor_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<FloorInput>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    validate_floor(&input).map_err(PortalError::bad_request)?;
    state
        .store()
        .update_floor(&floor_id, &input)
        .map_err(PortalError::bad_request)?;
    Ok(Json(json!({"id": floor_id})))
}

async fn upload_floor_plan(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<super::models::FloorPlanView>, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    let mut scope_type = String::new();
    let mut scope_id = String::new();
    let mut name = String::new();
    let mut source_file_name = "floor-plan.pdf".to_owned();
    let mut pdf_data = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|error| PortalError::bad_request(anyhow!(error)))?
    {
        let field_name = field.name().unwrap_or_default().to_owned();
        match field_name.as_str() {
            "scopeType" => {
                scope_type = field
                    .text()
                    .await
                    .map_err(|error| PortalError::bad_request(anyhow!(error)))?;
            }
            "scopeId" => {
                scope_id = field
                    .text()
                    .await
                    .map_err(|error| PortalError::bad_request(anyhow!(error)))?;
            }
            "name" => {
                name = field
                    .text()
                    .await
                    .map_err(|error| PortalError::bad_request(anyhow!(error)))?;
            }
            "pdf" => {
                if let Some(file_name) = field.file_name() {
                    source_file_name = clean_upload_name(file_name);
                }
                pdf_data = field
                    .bytes()
                    .await
                    .map_err(|error| PortalError::bad_request(anyhow!(error)))?
                    .to_vec();
            }
            _ => {}
        }
    }
    if !matches!(scope_type.as_str(), "building" | "floor")
        || scope_id.trim().is_empty()
        || scope_id.len() > 80
    {
        return Err(PortalError::bad_request(anyhow!(
            "valid building or floor scope is required"
        )));
    }
    validate_plan_name(&name).map_err(PortalError::bad_request)?;
    let (processed, pdf_data) = tokio::task::spawn_blocking(move || {
        let processed = process_pdf(&pdf_data)?;
        Ok::<_, anyhow::Error>((processed, pdf_data))
    })
    .await
    .map_err(|error| PortalError::internal(anyhow!("PDF task failed: {error}")))?
    .map_err(PortalError::bad_request)?;
    state
        .store()
        .save_floor_plan(SaveFloorPlan {
            scope_type: &scope_type,
            scope_id: &scope_id,
            name: &name,
            source_file_name: &source_file_name,
            processed: &processed,
            updated_by: &session.user.id,
            pdf_data: &pdf_data,
        })
        .map(Json)
        .map_err(PortalError::bad_request)
}

async fn update_floor_plan_trace(
    Path(plan_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<TraceUpdate>,
) -> Result<Json<super::models::FloorPlanView>, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    validate_trace(&input.trace).map_err(PortalError::bad_request)?;
    state
        .store()
        .update_floor_plan_trace(&plan_id, &input.trace, &session.user.id)
        .map(Json)
        .map_err(PortalError::bad_request)
}

async fn create_region(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<RegionInput>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    validate_region(&input).map_err(PortalError::bad_request)?;
    let id = state
        .store()
        .create_region(&input)
        .map_err(PortalError::bad_request)?;
    Ok(Json(json!({"id": id})))
}

async fn update_region(
    Path(region_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(input): Json<RegionInput>,
) -> Result<Json<serde_json::Value>, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    validate_region(&input).map_err(PortalError::bad_request)?;
    state
        .store()
        .update_region(&region_id, &input)
        .map_err(PortalError::bad_request)?;
    Ok(Json(json!({"id": region_id})))
}

async fn delete_region(
    Path(region_id): Path<String>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<StatusCode, PortalError> {
    let session = require_admin(&state, &headers)?;
    require_request_csrf(&headers, &session)?;
    state
        .store()
        .delete_region(&region_id)
        .map_err(PortalError::bad_request)?;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) fn require_authenticated_role(
    state: &AppState,
    headers: &HeaderMap,
    roles: &[PortalRole],
) -> Result<PortalSession, PortalError> {
    let session = require_session(state, headers)?;
    if roles.contains(&session.user.role) {
        Ok(session)
    } else {
        Err(PortalError::forbidden())
    }
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<PortalSession, PortalError> {
    require_authenticated_role(state, headers, &[PortalRole::Admin])
}

fn require_session(state: &AppState, headers: &HeaderMap) -> Result<PortalSession, PortalError> {
    let token = cookie_value(headers, SESSION_COOKIE)
        .ok_or_else(|| PortalError::unauthorized("Sign in to continue"))?;
    state
        .store()
        .portal_session(&token_hash(token))
        .map_err(PortalError::internal)?
        .ok_or_else(|| PortalError::unauthorized("Session expired; sign in again"))
}

pub(crate) fn require_request_csrf(
    headers: &HeaderMap,
    session: &PortalSession,
) -> Result<(), PortalError> {
    require_same_site(headers)?;
    let supplied = headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if supplied.len() == session.csrf_token.len() && supplied == session.csrf_token {
        Ok(())
    } else {
        Err(PortalError::forbidden())
    }
}

fn require_same_site(headers: &HeaderMap) -> Result<(), PortalError> {
    if let Some(site) = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        && !matches!(site, "same-origin" | "same-site" | "none")
    {
        return Err(PortalError::forbidden());
    }
    Ok(())
}

fn local_bootstrap_request(peer: SocketAddr, headers: &HeaderMap) -> bool {
    peer.ip().is_loopback()
        && headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .is_some_and(local_host_header)
}

fn require_local_configuration_access(
    peer: SocketAddr,
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<PortalSession>, PortalError> {
    if !local_bootstrap_request(peer, headers) {
        return Err(PortalError::local_only(anyhow!(
            "Metasys configuration request rejected from {peer}"
        )));
    }
    if state
        .store()
        .portal_user_count()
        .map_err(PortalError::internal)?
        == 0
    {
        Ok(None)
    } else {
        require_admin(state, headers).map(Some)
    }
}

fn local_host_header(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    ["localhost", "127.0.0.1", "[::1]"].iter().any(|candidate| {
        host == *candidate
            || host.strip_prefix(candidate).is_some_and(|suffix| {
                suffix.strip_prefix(':').is_some_and(|port| {
                    !port.is_empty() && port.chars().all(|c| c.is_ascii_digit())
                })
            })
    })
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then_some(value))
}

fn request_is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("https"))
}

fn session_cookie(token: &str, secure: bool) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=28800{}",
        if secure { "; Secure" } else { "" }
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

fn binary_response(
    data: Vec<u8>,
    content_type: &'static str,
    file_name: Option<&str>,
) -> Result<Response, PortalError> {
    let mut response = Response::new(Body::from(data));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Some(file_name) = file_name {
        let safe_name = clean_upload_name(file_name).replace('"', "");
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("inline; filename=\"{safe_name}\""))
                .map_err(PortalError::internal)?,
        );
    }
    Ok(response)
}

pub(crate) struct PortalError {
    status: StatusCode,
    public_message: String,
    source: Option<Box<anyhow::Error>>,
}

impl PortalError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            public_message: message.into(),
            source: None,
        }
    }

    fn unauthorized(message: &'static str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "This account is not allowed to perform that action",
        )
    }

    fn forbidden_from(source: anyhow::Error) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            public_message: "This floor plan is outside the assigned area".to_owned(),
            source: Some(Box::new(source)),
        }
    }

    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Resource was not found")
    }

    pub(crate) fn bad_request(source: anyhow::Error) -> Self {
        let public_message = source.to_string();
        Self {
            status: StatusCode::BAD_REQUEST,
            public_message,
            source: Some(Box::new(source)),
        }
    }

    fn internal(source: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            public_message: "The maintenance portal is temporarily unavailable".to_owned(),
            source: Some(Box::new(source.into())),
        }
    }

    pub(crate) fn bad_gateway(source: anyhow::Error) -> Self {
        let public_message = source.to_string();
        Self {
            status: StatusCode::BAD_GATEWAY,
            public_message,
            source: Some(Box::new(source)),
        }
    }

    pub(crate) fn dashboard(source: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            public_message: "Dashboard data is temporarily unavailable".to_owned(),
            source: Some(Box::new(source)),
        }
    }

    pub(crate) fn local_only(source: anyhow::Error) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            public_message: "Settings can only be changed from a browser running on this Mac"
                .to_owned(),
            source: Some(Box::new(source)),
        }
    }
}

impl IntoResponse for PortalError {
    fn into_response(self) -> Response {
        if let Some(source) = &self.source {
            tracing::warn!(error = %source, status = %self.status, "maintenance portal request failed");
        }
        (self.status, Json(json!({"error": self.public_message}))).into_response()
    }
}
