use std::{fmt, str::FromStr};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PortalRole {
    Admin,
    ViewOnly,
    Operator,
    ReportingStaff,
}

impl PortalRole {
    pub fn as_db(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::ViewOnly => "view_only",
            Self::Operator => "operator",
            Self::ReportingStaff => "reporting_staff",
        }
    }

    pub fn can_view_all(self) -> bool {
        matches!(self, Self::Admin | Self::Operator)
    }

    pub fn can_manage(self) -> bool {
        self == Self::Admin
    }

    pub fn can_report(self) -> bool {
        matches!(self, Self::Admin | Self::ReportingStaff)
    }

    pub fn can_note(self) -> bool {
        matches!(self, Self::Admin | Self::Operator)
    }
}

impl fmt::Display for PortalRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_db())
    }
}

impl FromStr for PortalRole {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "admin" => Ok(Self::Admin),
            "view_only" | "viewonly" | "view-only" => Ok(Self::ViewOnly),
            "operator" => Ok(Self::Operator),
            "reporting_staff" | "reportingstaff" | "reporting-staff" => Ok(Self::ReportingStaff),
            _ => bail!("unknown portal role"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PortalUserRecord {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub role: PortalRole,
    pub password_hash: String,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortalUserView {
    pub id: String,
    pub email: String,
    pub display_name: String,
    pub role: PortalRole,
    pub active: bool,
    pub floor_ids: Vec<String>,
    pub region_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct PortalSession {
    pub token_hash: String,
    pub csrf_token: String,
    pub user: PortalUserRecord,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub user: PortalUserView,
    pub csrf_token: String,
    pub initialized: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateUserRequest {
    pub email: String,
    pub display_name: String,
    pub role: PortalRole,
    pub password: String,
    #[serde(default)]
    pub floor_ids: Vec<String>,
    #[serde(default)]
    pub region_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateUserRequest {
    pub email: String,
    pub display_name: String,
    pub role: PortalRole,
    pub active: bool,
    pub password: Option<String>,
    #[serde(default)]
    pub floor_ids: Vec<String>,
    #[serde(default)]
    pub region_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedPoint {
    pub x: f32,
    pub y: f32,
}

impl NormalizedPoint {
    pub fn validate(&self) -> Result<()> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || !(0.0..=1.0).contains(&self.x)
            || !(0.0..=1.0).contains(&self.y)
        {
            bail!("map coordinates must be between 0 and 1");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TraceKind {
    Wall,
    Door,
    Cubicle,
    Furniture,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceFeature {
    pub id: String,
    pub kind: TraceKind,
    pub points: Vec<NormalizedPoint>,
    pub thickness: f32,
}

impl TraceFeature {
    pub fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() || self.id.len() > 80 {
            bail!("trace feature ID is invalid");
        }
        if !(0.5..=12.0).contains(&self.thickness) || !self.thickness.is_finite() {
            bail!("trace feature thickness must be between 0.5 and 12");
        }
        if !(2..=256).contains(&self.points.len()) {
            bail!("trace features require 2 to 256 points");
        }
        self.points.iter().try_for_each(NormalizedPoint::validate)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorPlanView {
    pub id: String,
    pub scope_type: String,
    pub scope_id: String,
    pub name: String,
    pub source_file_name: String,
    pub image_url: String,
    pub pdf_url: String,
    pub width: u32,
    pub height: u32,
    pub trace: Vec<TraceFeature>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct FloorPlanRecord {
    pub view: FloorPlanView,
    pub pdf_data: Vec<u8>,
    pub image_data: Vec<u8>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceUpdate {
    pub trace: Vec<TraceFeature>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemperatureReading {
    pub value: Option<f64>,
    pub display_value: String,
    pub unit: String,
    pub status: String,
    pub observed_at: DateTime<Utc>,
    pub available: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TemperatureMapping {
    pub object_id: String,
    pub attribute_id: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionView {
    pub id: String,
    pub floor_id: String,
    pub name: String,
    pub color: String,
    pub polygon: Vec<NormalizedPoint>,
    pub fav_box: String,
    pub temperature: Option<TemperatureReading>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_mapping: Option<TemperatureMapping>,
}

#[derive(Clone, Debug)]
pub struct RegionRecord {
    pub id: String,
    pub floor_id: String,
    pub name: String,
    pub color: String,
    pub polygon: Vec<NormalizedPoint>,
    pub fav_box: String,
    pub metasys_object_id: String,
    pub metasys_attribute_id: String,
}

impl RegionRecord {
    pub fn view(&self, include_mapping: bool) -> RegionView {
        RegionView {
            id: self.id.clone(),
            floor_id: self.floor_id.clone(),
            name: self.name.clone(),
            color: self.color.clone(),
            polygon: self.polygon.clone(),
            fav_box: self.fav_box.clone(),
            temperature: None,
            temperature_mapping: include_mapping.then(|| TemperatureMapping {
                object_id: self.metasys_object_id.clone(),
                attribute_id: self.metasys_attribute_id.clone(),
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegionInput {
    pub floor_id: String,
    pub name: String,
    pub color: String,
    pub polygon: Vec<NormalizedPoint>,
    #[serde(default)]
    pub fav_box: String,
    #[serde(default)]
    pub metasys_object_id: String,
    #[serde(default = "default_attribute_id")]
    pub metasys_attribute_id: String,
}

fn default_attribute_id() -> String {
    "85".to_owned()
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorView {
    pub id: String,
    pub building_id: String,
    pub name: String,
    pub sort_order: i64,
    pub floor_plan: Option<FloorPlanView>,
    pub regions: Vec<RegionView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildingView {
    pub id: String,
    pub name: String,
    pub sort_order: i64,
    pub overview_plan: Option<FloorPlanView>,
    pub floors: Vec<FloorView>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortalMapView {
    pub buildings: Vec<BuildingView>,
    pub can_manage: bool,
    pub can_report: bool,
    pub can_note: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildingInput {
    pub name: String,
    #[serde(default)]
    pub sort_order: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorInput {
    pub building_id: String,
    pub name: String,
    #[serde(default)]
    pub sort_order: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RequestStatus {
    Open,
    InProgress,
    Resolved,
    Closed,
}

impl RequestStatus {
    pub fn as_db(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Resolved => "resolved",
            Self::Closed => "closed",
        }
    }
}

impl FromStr for RequestStatus {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "open" => Ok(Self::Open),
            "in_progress" => Ok(Self::InProgress),
            "resolved" => Ok(Self::Resolved),
            "closed" => Ok(Self::Closed),
            _ => bail!("unknown service-request status"),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRequestNoteView {
    pub id: String,
    pub author_name: String,
    pub note: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRequestView {
    pub id: String,
    pub region_id: String,
    pub region_name: String,
    pub floor_name: String,
    pub building_name: String,
    pub created_by_name: String,
    pub contact_email: String,
    pub issue_type: String,
    pub details: String,
    pub status: RequestStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub notes: Vec<ServiceRequestNoteView>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateServiceRequest {
    pub region_id: String,
    pub contact_email: String,
    pub issue_type: String,
    #[serde(default)]
    pub details: String,
}

#[derive(Debug, Deserialize)]
pub struct AddNoteRequest {
    pub note: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRequestStatus {
    pub status: RequestStatus,
}
