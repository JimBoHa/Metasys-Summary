use std::{collections::HashSet, str::FromStr};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

use crate::store::Store;

use super::{
    floorplan::ProcessedFloorPlan,
    models::{
        BuildingInput, BuildingView, CreateServiceRequest, FloorInput, FloorPlanRecord,
        FloorPlanView, FloorView, PortalMapView, PortalRole, PortalSession, PortalUserRecord,
        PortalUserView, RegionInput, RegionRecord, RequestStatus, ServiceRequestNoteView,
        ServiceRequestView, TraceFeature, UpdateUserRequest,
    },
};

pub struct SaveFloorPlan<'a> {
    pub scope_type: &'a str,
    pub scope_id: &'a str,
    pub name: &'a str,
    pub source_file_name: &'a str,
    pub processed: &'a ProcessedFloorPlan,
    pub updated_by: &'a str,
    pub pdf_data: &'a [u8],
}

type RegionTuple = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
);

const SESSION_HOURS: i64 = 8;

pub fn initialize_schema(connection: &Connection) -> Result<()> {
    connection
        .execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS portal_users (
                id TEXT PRIMARY KEY,
                email TEXT NOT NULL COLLATE NOCASE UNIQUE,
                display_name TEXT NOT NULL,
                role TEXT NOT NULL CHECK(role IN ('admin', 'view_only', 'operator', 'reporting_staff')),
                password_hash TEXT NOT NULL,
                active INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS portal_sessions (
                token_hash TEXT PRIMARY KEY,
                user_id TEXT NOT NULL REFERENCES portal_users(id) ON DELETE CASCADE,
                csrf_token TEXT NOT NULL,
                peer_address TEXT NOT NULL,
                created_at TEXT NOT NULL,
                last_seen_at TEXT NOT NULL,
                expires_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_portal_sessions_expires
                ON portal_sessions(expires_at);

            CREATE TABLE IF NOT EXISTS portal_login_attempts (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                email TEXT NOT NULL COLLATE NOCASE,
                peer_address TEXT NOT NULL,
                attempted_at TEXT NOT NULL,
                successful INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_portal_login_attempts_time
                ON portal_login_attempts(attempted_at);

            CREATE TABLE IF NOT EXISTS portal_buildings (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                sort_order INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS portal_floors (
                id TEXT PRIMARY KEY,
                building_id TEXT NOT NULL REFERENCES portal_buildings(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                sort_order INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_portal_floors_building
                ON portal_floors(building_id, sort_order);

            CREATE TABLE IF NOT EXISTS portal_floorplans (
                id TEXT PRIMARY KEY,
                scope_type TEXT NOT NULL CHECK(scope_type IN ('building', 'floor')),
                scope_id TEXT NOT NULL,
                name TEXT NOT NULL,
                source_file_name TEXT NOT NULL,
                pdf_data BLOB NOT NULL,
                image_data BLOB NOT NULL,
                width INTEGER NOT NULL,
                height INTEGER NOT NULL,
                trace_json TEXT NOT NULL,
                updated_by TEXT NOT NULL REFERENCES portal_users(id),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE(scope_type, scope_id)
            );

            CREATE TABLE IF NOT EXISTS portal_regions (
                id TEXT PRIMARY KEY,
                floor_id TEXT NOT NULL REFERENCES portal_floors(id) ON DELETE CASCADE,
                name TEXT NOT NULL,
                color TEXT NOT NULL,
                polygon_json TEXT NOT NULL,
                fav_box TEXT NOT NULL DEFAULT '',
                metasys_object_id TEXT NOT NULL DEFAULT '',
                metasys_attribute_id TEXT NOT NULL DEFAULT '85',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_portal_regions_floor
                ON portal_regions(floor_id);

            CREATE TABLE IF NOT EXISTS portal_user_floors (
                user_id TEXT NOT NULL REFERENCES portal_users(id) ON DELETE CASCADE,
                floor_id TEXT NOT NULL REFERENCES portal_floors(id) ON DELETE CASCADE,
                PRIMARY KEY(user_id, floor_id)
            );

            CREATE TABLE IF NOT EXISTS portal_user_regions (
                user_id TEXT NOT NULL REFERENCES portal_users(id) ON DELETE CASCADE,
                region_id TEXT NOT NULL REFERENCES portal_regions(id) ON DELETE CASCADE,
                PRIMARY KEY(user_id, region_id)
            );

            CREATE TABLE IF NOT EXISTS portal_service_requests (
                id TEXT PRIMARY KEY,
                region_id TEXT NOT NULL REFERENCES portal_regions(id),
                created_by TEXT NOT NULL REFERENCES portal_users(id),
                contact_email TEXT NOT NULL,
                issue_type TEXT NOT NULL,
                details TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('open', 'in_progress', 'resolved', 'closed')),
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_portal_requests_region
                ON portal_service_requests(region_id, created_at DESC);

            CREATE TABLE IF NOT EXISTS portal_service_request_notes (
                id TEXT PRIMARY KEY,
                request_id TEXT NOT NULL REFERENCES portal_service_requests(id) ON DELETE CASCADE,
                author_id TEXT NOT NULL REFERENCES portal_users(id),
                note TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_portal_notes_request
                ON portal_service_request_notes(request_id, created_at);
            "#,
        )
        .context("initialize maintenance portal schema")?;
    Ok(())
}

impl Store {
    pub fn portal_user_count(&self) -> Result<usize> {
        let connection = self.lock()?;
        let count = connection.query_row("SELECT COUNT(*) FROM portal_users", [], |row| {
            row.get::<_, i64>(0)
        })?;
        Ok(count.max(0) as usize)
    }

    pub fn create_portal_user(
        &self,
        email: &str,
        display_name: &str,
        role: PortalRole,
        password_hash: &str,
        floor_ids: &[String],
        region_ids: &[String],
    ) -> Result<PortalUserView> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        transaction
            .execute(
                r#"
                INSERT INTO portal_users (
                    id, email, display_name, role, password_hash, active, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6)
                "#,
                params![
                    id,
                    email.trim().to_ascii_lowercase(),
                    display_name.trim(),
                    role.as_db(),
                    password_hash,
                    now,
                ],
            )
            .context("create portal user")?;
        replace_user_scopes(&transaction, &id, floor_ids, region_ids)?;
        transaction.commit()?;
        drop(connection);
        self.portal_user_view(&id)
    }

    pub fn portal_user_for_login(&self, email: &str) -> Result<Option<PortalUserRecord>> {
        let connection = self.lock()?;
        let tuple: Option<(String, String, String, String, String, bool)> = connection
            .query_row(
                r#"
                SELECT id, email, display_name, role, password_hash, active
                FROM portal_users
                WHERE email = ?1 COLLATE NOCASE
                "#,
                [email.trim()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        tuple.map(user_record_from_tuple).transpose()
    }

    pub fn portal_user_view(&self, user_id: &str) -> Result<PortalUserView> {
        let connection = self.lock()?;
        portal_user_view_with_connection(&connection, user_id)
    }

    pub fn list_portal_users(&self) -> Result<Vec<PortalUserView>> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT id FROM portal_users ORDER BY display_name COLLATE NOCASE, email COLLATE NOCASE",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter()
            .map(|id| portal_user_view_with_connection(&connection, id))
            .collect()
    }

    pub fn update_portal_user(
        &self,
        user_id: &str,
        update: &UpdateUserRequest,
        password_hash: Option<&str>,
    ) -> Result<PortalUserView> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let changed = if let Some(password_hash) = password_hash {
            transaction.execute(
                r#"
                UPDATE portal_users
                SET email = ?2, display_name = ?3, role = ?4, active = ?5,
                    password_hash = ?6, updated_at = ?7
                WHERE id = ?1
                "#,
                params![
                    user_id,
                    update.email.trim().to_ascii_lowercase(),
                    update.display_name.trim(),
                    update.role.as_db(),
                    update.active,
                    password_hash,
                    Utc::now().to_rfc3339(),
                ],
            )?
        } else {
            transaction.execute(
                r#"
                UPDATE portal_users
                SET email = ?2, display_name = ?3, role = ?4, active = ?5, updated_at = ?6
                WHERE id = ?1
                "#,
                params![
                    user_id,
                    update.email.trim().to_ascii_lowercase(),
                    update.display_name.trim(),
                    update.role.as_db(),
                    update.active,
                    Utc::now().to_rfc3339(),
                ],
            )?
        };
        if changed == 0 {
            bail!("portal user was not found");
        }
        replace_user_scopes(&transaction, user_id, &update.floor_ids, &update.region_ids)?;
        if !update.active || password_hash.is_some() {
            transaction.execute("DELETE FROM portal_sessions WHERE user_id = ?1", [user_id])?;
        }
        transaction.commit()?;
        drop(connection);
        self.portal_user_view(user_id)
    }

    pub fn login_is_rate_limited(&self, email: &str, peer: &str) -> Result<bool> {
        let connection = self.lock()?;
        let cutoff = (Utc::now() - Duration::minutes(15)).to_rfc3339();
        let failures: i64 = connection.query_row(
            r#"
            SELECT COUNT(*) FROM portal_login_attempts
            WHERE successful = 0 AND attempted_at >= ?1
              AND (email = ?2 COLLATE NOCASE OR peer_address = ?3)
            "#,
            params![cutoff, email.trim(), peer],
            |row| row.get(0),
        )?;
        Ok(failures >= 8)
    }

    pub fn record_login_attempt(&self, email: &str, peer: &str, successful: bool) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO portal_login_attempts (email, peer_address, attempted_at, successful)
            VALUES (?1, ?2, ?3, ?4)
            "#,
            params![
                email.trim().to_ascii_lowercase(),
                peer,
                Utc::now().to_rfc3339(),
                successful,
            ],
        )?;
        connection.execute(
            "DELETE FROM portal_login_attempts WHERE attempted_at < ?1",
            [(Utc::now() - Duration::hours(24)).to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn create_portal_session(
        &self,
        user_id: &str,
        token_hash: &str,
        csrf_token: &str,
        peer: &str,
    ) -> Result<()> {
        let now = Utc::now();
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO portal_sessions (
                token_hash, user_id, csrf_token, peer_address,
                created_at, last_seen_at, expires_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
            "#,
            params![
                token_hash,
                user_id,
                csrf_token,
                peer,
                now.to_rfc3339(),
                (now + Duration::hours(SESSION_HOURS)).to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn portal_session(&self, token_hash: &str) -> Result<Option<PortalSession>> {
        let connection = self.lock()?;
        connection.execute(
            "DELETE FROM portal_sessions WHERE expires_at <= ?1",
            [Utc::now().to_rfc3339()],
        )?;
        let tuple: Option<(String, String, String, String, String, String, bool)> = connection
            .query_row(
                r#"
                SELECT s.csrf_token, u.id, u.email, u.display_name, u.role,
                       u.password_hash, u.active
                FROM portal_sessions s
                JOIN portal_users u ON u.id = s.user_id
                WHERE s.token_hash = ?1 AND s.expires_at > ?2 AND u.active = 1
                "#,
                params![token_hash, Utc::now().to_rfc3339()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((csrf_token, id, email, display_name, role, password_hash, active)) = tuple else {
            return Ok(None);
        };
        connection.execute(
            "UPDATE portal_sessions SET last_seen_at = ?2 WHERE token_hash = ?1",
            params![token_hash, Utc::now().to_rfc3339()],
        )?;
        Ok(Some(PortalSession {
            token_hash: token_hash.to_owned(),
            csrf_token,
            user: user_record_from_tuple((id, email, display_name, role, password_hash, active))?,
        }))
    }

    pub fn delete_portal_session(&self, token_hash: &str) -> Result<()> {
        let connection = self.lock()?;
        connection.execute(
            "DELETE FROM portal_sessions WHERE token_hash = ?1",
            [token_hash],
        )?;
        Ok(())
    }

    pub fn create_building(&self, input: &BuildingInput) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO portal_buildings (id, name, sort_order, created_at, updated_at)
            VALUES (?1, ?2, ?3, ?4, ?4)
            "#,
            params![id, input.name.trim(), input.sort_order, now],
        )?;
        Ok(id)
    }

    pub fn update_building(&self, building_id: &str, input: &BuildingInput) -> Result<()> {
        let connection = self.lock()?;
        let changed = connection.execute(
            r#"
            UPDATE portal_buildings
            SET name = ?2, sort_order = ?3, updated_at = ?4
            WHERE id = ?1
            "#,
            params![
                building_id,
                input.name.trim(),
                input.sort_order,
                Utc::now().to_rfc3339(),
            ],
        )?;
        if changed == 0 {
            bail!("building was not found");
        }
        Ok(())
    }

    pub fn create_floor(&self, input: &FloorInput) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO portal_floors (
                id, building_id, name, sort_order, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
            "#,
            params![
                id,
                input.building_id,
                input.name.trim(),
                input.sort_order,
                now,
            ],
        )?;
        Ok(id)
    }

    pub fn update_floor(&self, floor_id: &str, input: &FloorInput) -> Result<()> {
        let connection = self.lock()?;
        let changed = connection.execute(
            r#"
            UPDATE portal_floors
            SET building_id = ?2, name = ?3, sort_order = ?4, updated_at = ?5
            WHERE id = ?1
            "#,
            params![
                floor_id,
                input.building_id,
                input.name.trim(),
                input.sort_order,
                Utc::now().to_rfc3339(),
            ],
        )?;
        if changed == 0 {
            bail!("floor was not found");
        }
        Ok(())
    }

    pub fn save_floor_plan(&self, input: SaveFloorPlan<'_>) -> Result<FloorPlanView> {
        {
            let connection = self.lock()?;
            validate_floor_plan_scope(&connection, input.scope_type, input.scope_id)?;
        }
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let trace_json = serde_json::to_string(&input.processed.trace)?;
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO portal_floorplans (
                id, scope_type, scope_id, name, source_file_name, pdf_data, image_data,
                width, height, trace_json, updated_by, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?12)
            ON CONFLICT(scope_type, scope_id) DO UPDATE SET
                name = excluded.name,
                source_file_name = excluded.source_file_name,
                pdf_data = excluded.pdf_data,
                image_data = excluded.image_data,
                width = excluded.width,
                height = excluded.height,
                trace_json = excluded.trace_json,
                updated_by = excluded.updated_by,
                updated_at = excluded.updated_at
            "#,
            params![
                id,
                input.scope_type,
                input.scope_id,
                input.name.trim(),
                input.source_file_name,
                input.pdf_data,
                input.processed.image_data,
                i64::from(input.processed.width),
                i64::from(input.processed.height),
                trace_json,
                input.updated_by,
                now,
            ],
        )?;
        floor_plan_for_scope(&connection, input.scope_type, input.scope_id)?
            .ok_or_else(|| anyhow!("saved floor plan could not be read"))
    }

    pub fn update_floor_plan_trace(
        &self,
        plan_id: &str,
        trace: &[TraceFeature],
        updated_by: &str,
    ) -> Result<FloorPlanView> {
        let connection = self.lock()?;
        let changed = connection.execute(
            r#"
            UPDATE portal_floorplans
            SET trace_json = ?2, updated_by = ?3, updated_at = ?4
            WHERE id = ?1
            "#,
            params![
                plan_id,
                serde_json::to_string(trace)?,
                updated_by,
                Utc::now().to_rfc3339(),
            ],
        )?;
        if changed == 0 {
            bail!("floor plan was not found");
        }
        floor_plan_by_id(&connection, plan_id)?
            .map(|record| record.view)
            .ok_or_else(|| anyhow!("updated floor plan could not be read"))
    }

    pub fn floor_plan_data_for_user(
        &self,
        plan_id: &str,
        user: &PortalUserRecord,
    ) -> Result<FloorPlanRecord> {
        let connection = self.lock()?;
        let record = floor_plan_by_id(&connection, plan_id)?
            .ok_or_else(|| anyhow!("floor plan was not found"))?;
        let allowed = if user.role.can_view_all() {
            true
        } else if record.view.scope_type == "floor" {
            user_can_access_floor(&connection, &user.id, &record.view.scope_id)?
        } else {
            user_can_access_building(&connection, &user.id, &record.view.scope_id)?
        };
        if !allowed {
            bail!("floor plan is outside the assigned area");
        }
        Ok(record)
    }

    pub fn create_region(&self, input: &RegionInput) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO portal_regions (
                id, floor_id, name, color, polygon_json, fav_box,
                metasys_object_id, metasys_attribute_id, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
            "#,
            params![
                id,
                input.floor_id,
                input.name.trim(),
                input.color,
                serde_json::to_string(&input.polygon)?,
                input.fav_box.trim(),
                input.metasys_object_id.trim(),
                input.metasys_attribute_id.trim(),
                now,
            ],
        )?;
        Ok(id)
    }

    pub fn update_region(&self, region_id: &str, input: &RegionInput) -> Result<()> {
        let connection = self.lock()?;
        let changed = connection.execute(
            r#"
            UPDATE portal_regions
            SET floor_id = ?2, name = ?3, color = ?4, polygon_json = ?5,
                fav_box = ?6, metasys_object_id = ?7, metasys_attribute_id = ?8,
                updated_at = ?9
            WHERE id = ?1
            "#,
            params![
                region_id,
                input.floor_id,
                input.name.trim(),
                input.color,
                serde_json::to_string(&input.polygon)?,
                input.fav_box.trim(),
                input.metasys_object_id.trim(),
                input.metasys_attribute_id.trim(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        if changed == 0 {
            bail!("region was not found");
        }
        Ok(())
    }

    pub fn delete_region(&self, region_id: &str) -> Result<()> {
        let connection = self.lock()?;
        let requests: i64 = connection.query_row(
            "SELECT COUNT(*) FROM portal_service_requests WHERE region_id = ?1",
            [region_id],
            |row| row.get(0),
        )?;
        if requests > 0 {
            bail!("region has service requests and cannot be deleted");
        }
        let changed =
            connection.execute("DELETE FROM portal_regions WHERE id = ?1", [region_id])?;
        if changed == 0 {
            bail!("region was not found");
        }
        Ok(())
    }

    pub fn portal_map(&self, user: &PortalUserRecord) -> Result<PortalMapView> {
        let connection = self.lock()?;
        let (allowed_floors, allowed_regions) = accessible_ids(&connection, user)?;
        let mut building_statement = connection.prepare(
            "SELECT id, name, sort_order FROM portal_buildings ORDER BY sort_order, name COLLATE NOCASE",
        )?;
        let building_rows = building_statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut buildings = Vec::new();
        for (building_id, name, sort_order) in building_rows {
            let mut floor_statement = connection.prepare(
                r#"
                SELECT id, name, sort_order FROM portal_floors
                WHERE building_id = ?1 ORDER BY sort_order, name COLLATE NOCASE
                "#,
            )?;
            let floor_rows = floor_statement
                .query_map([&building_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut floors = Vec::new();
            for (floor_id, floor_name, floor_order) in floor_rows {
                if !user.role.can_view_all() && !allowed_floors.contains(&floor_id) {
                    continue;
                }
                let all_floor_regions = regions_for_floor(&connection, &floor_id)?;
                let regions = all_floor_regions
                    .into_iter()
                    .filter(|region| {
                        user.role.can_view_all()
                            || floor_is_directly_assigned(&connection, &user.id, &floor_id)
                                .unwrap_or(false)
                            || allowed_regions.contains(&region.id)
                    })
                    .map(|region| region.view(user.role.can_manage()))
                    .collect();
                floors.push(FloorView {
                    id: floor_id.clone(),
                    building_id: building_id.clone(),
                    name: floor_name,
                    sort_order: floor_order,
                    floor_plan: floor_plan_for_scope(&connection, "floor", &floor_id)?,
                    regions,
                });
            }
            if floors.is_empty() && !user.role.can_view_all() {
                continue;
            }
            buildings.push(BuildingView {
                id: building_id.clone(),
                name,
                sort_order,
                overview_plan: floor_plan_for_scope(&connection, "building", &building_id)?,
                floors,
            });
        }
        Ok(PortalMapView {
            buildings,
            can_manage: user.role.can_manage(),
            can_report: user.role.can_report(),
            can_note: user.role.can_note(),
        })
    }

    pub fn user_can_access_region(&self, user: &PortalUserRecord, region_id: &str) -> Result<bool> {
        if user.role.can_view_all() {
            return Ok(true);
        }
        let connection = self.lock()?;
        let floor_id: Option<String> = connection
            .query_row(
                "SELECT floor_id FROM portal_regions WHERE id = ?1",
                [region_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(floor_id) = floor_id else {
            return Ok(false);
        };
        Ok(
            floor_is_directly_assigned(&connection, &user.id, &floor_id)?
                || region_is_assigned(&connection, &user.id, region_id)?,
        )
    }

    pub fn region_record(&self, region_id: &str) -> Result<Option<RegionRecord>> {
        let connection = self.lock()?;
        region_by_id(&connection, region_id)
    }

    pub fn create_service_request(
        &self,
        user_id: &str,
        input: &CreateServiceRequest,
    ) -> Result<ServiceRequestView> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO portal_service_requests (
                id, region_id, created_by, contact_email, issue_type, details,
                status, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?7)
            "#,
            params![
                id,
                input.region_id,
                user_id,
                input.contact_email.trim().to_ascii_lowercase(),
                input.issue_type,
                input.details.trim(),
                now,
            ],
        )?;
        service_request_by_id(&connection, &id)?
            .ok_or_else(|| anyhow!("created service request could not be read"))
    }

    pub fn list_service_requests(
        &self,
        user: &PortalUserRecord,
    ) -> Result<Vec<ServiceRequestView>> {
        let connection = self.lock()?;
        let (_, allowed_regions) = accessible_ids(&connection, user)?;
        let directly_assigned_floors = direct_floor_ids(&connection, &user.id)?;
        let mut statement = connection.prepare(
            r#"
            SELECT r.id, r.region_id
            FROM portal_service_requests r
            ORDER BY r.created_at DESC
            LIMIT 500
            "#,
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .filter(|(_, region_id)| {
                if user.role.can_view_all() || allowed_regions.contains(region_id) {
                    return true;
                }
                region_by_id(&connection, region_id)
                    .ok()
                    .flatten()
                    .is_some_and(|region| directly_assigned_floors.contains(&region.floor_id))
            })
            .filter_map(
                |(request_id, _)| match service_request_by_id(&connection, &request_id) {
                    Ok(Some(request)) => Some(Ok(request)),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                },
            )
            .collect()
    }

    pub fn add_service_request_note(
        &self,
        request_id: &str,
        author_id: &str,
        note: &str,
    ) -> Result<ServiceRequestView> {
        let connection = self.lock()?;
        connection.execute(
            r#"
            INSERT INTO portal_service_request_notes (
                id, request_id, author_id, note, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            params![
                Uuid::new_v4().to_string(),
                request_id,
                author_id,
                note.trim(),
                Utc::now().to_rfc3339(),
            ],
        )?;
        connection.execute(
            "UPDATE portal_service_requests SET updated_at = ?2 WHERE id = ?1",
            params![request_id, Utc::now().to_rfc3339()],
        )?;
        service_request_by_id(&connection, request_id)?
            .ok_or_else(|| anyhow!("service request was not found"))
    }

    pub fn update_service_request_status(
        &self,
        request_id: &str,
        status: RequestStatus,
    ) -> Result<ServiceRequestView> {
        let connection = self.lock()?;
        let changed = connection.execute(
            "UPDATE portal_service_requests SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![request_id, status.as_db(), Utc::now().to_rfc3339()],
        )?;
        if changed == 0 {
            bail!("service request was not found");
        }
        service_request_by_id(&connection, request_id)?
            .ok_or_else(|| anyhow!("updated service request could not be read"))
    }
}

fn user_record_from_tuple(
    tuple: (String, String, String, String, String, bool),
) -> Result<PortalUserRecord> {
    Ok(PortalUserRecord {
        id: tuple.0,
        email: tuple.1,
        display_name: tuple.2,
        role: PortalRole::from_str(&tuple.3)?,
        password_hash: tuple.4,
        active: tuple.5,
    })
}

fn portal_user_view_with_connection(
    connection: &Connection,
    user_id: &str,
) -> Result<PortalUserView> {
    let tuple: (String, String, String, String, bool) = connection
        .query_row(
            "SELECT id, email, display_name, role, active FROM portal_users WHERE id = ?1",
            [user_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .context("portal user was not found")?;
    Ok(PortalUserView {
        id: tuple.0,
        email: tuple.1,
        display_name: tuple.2,
        role: PortalRole::from_str(&tuple.3)?,
        active: tuple.4,
        floor_ids: direct_floor_ids(connection, user_id)?.into_iter().collect(),
        region_ids: direct_region_ids(connection, user_id)?
            .into_iter()
            .collect(),
    })
}

fn replace_user_scopes(
    transaction: &rusqlite::Transaction<'_>,
    user_id: &str,
    floor_ids: &[String],
    region_ids: &[String],
) -> Result<()> {
    transaction.execute(
        "DELETE FROM portal_user_floors WHERE user_id = ?1",
        [user_id],
    )?;
    transaction.execute(
        "DELETE FROM portal_user_regions WHERE user_id = ?1",
        [user_id],
    )?;
    for floor_id in floor_ids.iter().collect::<HashSet<_>>() {
        transaction.execute(
            "INSERT INTO portal_user_floors (user_id, floor_id) VALUES (?1, ?2)",
            params![user_id, floor_id],
        )?;
    }
    for region_id in region_ids.iter().collect::<HashSet<_>>() {
        transaction.execute(
            "INSERT INTO portal_user_regions (user_id, region_id) VALUES (?1, ?2)",
            params![user_id, region_id],
        )?;
    }
    Ok(())
}

fn direct_floor_ids(connection: &Connection, user_id: &str) -> Result<HashSet<String>> {
    let mut statement =
        connection.prepare("SELECT floor_id FROM portal_user_floors WHERE user_id = ?1")?;
    Ok(statement
        .query_map([user_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?)
}

fn direct_region_ids(connection: &Connection, user_id: &str) -> Result<HashSet<String>> {
    let mut statement =
        connection.prepare("SELECT region_id FROM portal_user_regions WHERE user_id = ?1")?;
    Ok(statement
        .query_map([user_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?)
}

fn accessible_ids(
    connection: &Connection,
    user: &PortalUserRecord,
) -> Result<(HashSet<String>, HashSet<String>)> {
    if user.role.can_view_all() {
        let mut floor_statement = connection.prepare("SELECT id FROM portal_floors")?;
        let floors = floor_statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        let mut region_statement = connection.prepare("SELECT id FROM portal_regions")?;
        let regions = region_statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        return Ok((floors, regions));
    }
    let mut floors = direct_floor_ids(connection, &user.id)?;
    let regions = direct_region_ids(connection, &user.id)?;
    for region_id in &regions {
        if let Some(region) = region_by_id(connection, region_id)? {
            floors.insert(region.floor_id);
        }
    }
    Ok((floors, regions))
}

fn floor_is_directly_assigned(
    connection: &Connection,
    user_id: &str,
    floor_id: &str,
) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM portal_user_floors WHERE user_id = ?1 AND floor_id = ?2",
            params![user_id, floor_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn region_is_assigned(connection: &Connection, user_id: &str, region_id: &str) -> Result<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM portal_user_regions WHERE user_id = ?1 AND region_id = ?2",
            params![user_id, region_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn user_can_access_floor(connection: &Connection, user_id: &str, floor_id: &str) -> Result<bool> {
    if floor_is_directly_assigned(connection, user_id, floor_id)? {
        return Ok(true);
    }
    Ok(connection
        .query_row(
            r#"
            SELECT 1 FROM portal_user_regions ur
            JOIN portal_regions r ON r.id = ur.region_id
            WHERE ur.user_id = ?1 AND r.floor_id = ?2 LIMIT 1
            "#,
            params![user_id, floor_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn user_can_access_building(
    connection: &Connection,
    user_id: &str,
    building_id: &str,
) -> Result<bool> {
    Ok(connection
        .query_row(
            r#"
            SELECT 1 FROM portal_floors f
            WHERE f.building_id = ?2 AND (
                EXISTS (
                    SELECT 1 FROM portal_user_floors uf
                    WHERE uf.user_id = ?1 AND uf.floor_id = f.id
                ) OR EXISTS (
                    SELECT 1 FROM portal_user_regions ur
                    JOIN portal_regions r ON r.id = ur.region_id
                    WHERE ur.user_id = ?1 AND r.floor_id = f.id
                )
            ) LIMIT 1
            "#,
            params![user_id, building_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn validate_floor_plan_scope(
    connection: &Connection,
    scope_type: &str,
    scope_id: &str,
) -> Result<()> {
    let table = match scope_type {
        "building" => "portal_buildings",
        "floor" => "portal_floors",
        _ => bail!("floor-plan scope must be building or floor"),
    };
    let sql = format!("SELECT 1 FROM {table} WHERE id = ?1");
    if connection
        .query_row(&sql, [scope_id], |_| Ok(()))
        .optional()?
        .is_none()
    {
        bail!("floor-plan scope was not found");
    }
    Ok(())
}

fn floor_plan_for_scope(
    connection: &Connection,
    scope_type: &str,
    scope_id: &str,
) -> Result<Option<FloorPlanView>> {
    let id: Option<String> = connection
        .query_row(
            "SELECT id FROM portal_floorplans WHERE scope_type = ?1 AND scope_id = ?2",
            params![scope_type, scope_id],
            |row| row.get(0),
        )
        .optional()?;
    id.map(|id| floor_plan_by_id(connection, &id)?.ok_or_else(|| anyhow!("floor plan disappeared")))
        .transpose()
        .map(|record| record.map(|record| record.view))
}

fn floor_plan_by_id(connection: &Connection, plan_id: &str) -> Result<Option<FloorPlanRecord>> {
    type PlanTuple = (
        String,
        String,
        String,
        String,
        String,
        Vec<u8>,
        Vec<u8>,
        i64,
        i64,
        String,
        String,
    );
    let tuple: Option<PlanTuple> = connection
        .query_row(
            r#"
            SELECT id, scope_type, scope_id, name, source_file_name, pdf_data, image_data,
                   width, height, trace_json, updated_at
            FROM portal_floorplans WHERE id = ?1
            "#,
            [plan_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                ))
            },
        )
        .optional()?;
    let Some(tuple) = tuple else {
        return Ok(None);
    };
    let trace = serde_json::from_str(&tuple.9).context("decode stored floor-plan trace")?;
    Ok(Some(FloorPlanRecord {
        view: FloorPlanView {
            id: tuple.0.clone(),
            scope_type: tuple.1,
            scope_id: tuple.2,
            name: tuple.3,
            source_file_name: tuple.4,
            image_url: format!("/api/portal/floorplans/{}/image", tuple.0),
            pdf_url: format!("/api/portal/floorplans/{}/pdf", tuple.0),
            width: tuple.7.max(1) as u32,
            height: tuple.8.max(1) as u32,
            trace,
            updated_at: parse_datetime(&tuple.10),
        },
        pdf_data: tuple.5,
        image_data: tuple.6,
    }))
}

fn regions_for_floor(connection: &Connection, floor_id: &str) -> Result<Vec<RegionRecord>> {
    let mut statement = connection.prepare(
        r#"
        SELECT id, floor_id, name, color, polygon_json, fav_box,
               metasys_object_id, metasys_attribute_id
        FROM portal_regions WHERE floor_id = ?1 ORDER BY name COLLATE NOCASE
        "#,
    )?;
    let rows = statement
        .query_map([floor_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|row| {
            Ok(RegionRecord {
                id: row.0,
                floor_id: row.1,
                name: row.2,
                color: row.3,
                polygon: serde_json::from_str(&row.4).context("decode region polygon")?,
                fav_box: row.5,
                metasys_object_id: row.6,
                metasys_attribute_id: row.7,
            })
        })
        .collect()
}

fn region_by_id(connection: &Connection, region_id: &str) -> Result<Option<RegionRecord>> {
    let tuple: Option<RegionTuple> = connection
        .query_row(
            r#"
            SELECT id, floor_id, name, color, polygon_json, fav_box,
                   metasys_object_id, metasys_attribute_id
            FROM portal_regions WHERE id = ?1
            "#,
            [region_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()?;
    tuple
        .map(|row| {
            Ok(RegionRecord {
                id: row.0,
                floor_id: row.1,
                name: row.2,
                color: row.3,
                polygon: serde_json::from_str(&row.4).context("decode region polygon")?,
                fav_box: row.5,
                metasys_object_id: row.6,
                metasys_attribute_id: row.7,
            })
        })
        .transpose()
}

fn service_request_by_id(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<ServiceRequestView>> {
    type RequestTuple = (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    );
    let tuple: Option<RequestTuple> = connection
        .query_row(
            r#"
            SELECT sr.id, sr.region_id, r.name, f.name, b.name, u.display_name,
                   sr.contact_email, sr.issue_type, sr.details, sr.status,
                   sr.created_at, sr.updated_at
            FROM portal_service_requests sr
            JOIN portal_regions r ON r.id = sr.region_id
            JOIN portal_floors f ON f.id = r.floor_id
            JOIN portal_buildings b ON b.id = f.building_id
            JOIN portal_users u ON u.id = sr.created_by
            WHERE sr.id = ?1
            "#,
            [request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            },
        )
        .optional()?;
    let Some(tuple) = tuple else {
        return Ok(None);
    };
    let mut notes_statement = connection.prepare(
        r#"
        SELECT n.id, u.display_name, n.note, n.created_at
        FROM portal_service_request_notes n
        JOIN portal_users u ON u.id = n.author_id
        WHERE n.request_id = ?1 ORDER BY n.created_at
        "#,
    )?;
    let note_rows = notes_statement
        .query_map([request_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let notes = note_rows
        .into_iter()
        .map(|row| ServiceRequestNoteView {
            id: row.0,
            author_name: row.1,
            note: row.2,
            created_at: parse_datetime(&row.3),
        })
        .collect();
    Ok(Some(ServiceRequestView {
        id: tuple.0,
        region_id: tuple.1,
        region_name: tuple.2,
        floor_name: tuple.3,
        building_name: tuple.4,
        created_by_name: tuple.5,
        contact_email: tuple.6,
        issue_type: tuple.7,
        details: tuple.8,
        status: RequestStatus::from_str(&tuple.9)?,
        created_at: parse_datetime(&tuple.10),
        updated_at: parse_datetime(&tuple.11),
        notes,
    }))
}

fn parse_datetime(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn scoped_user_sees_only_assigned_floor() {
        let directory = tempdir().unwrap();
        let store = Store::open(&directory.path().join("portal.sqlite3")).unwrap();
        let building_id = store
            .create_building(&BuildingInput {
                name: "Building A".to_owned(),
                sort_order: 0,
            })
            .unwrap();
        let first_floor = store
            .create_floor(&FloorInput {
                building_id: building_id.clone(),
                name: "First floor".to_owned(),
                sort_order: 1,
            })
            .unwrap();
        store
            .create_floor(&FloorInput {
                building_id,
                name: "Second floor".to_owned(),
                sort_order: 2,
            })
            .unwrap();
        let user = store
            .create_portal_user(
                "viewer@example.invalid",
                "Viewer",
                PortalRole::ViewOnly,
                "hash",
                std::slice::from_ref(&first_floor),
                &[],
            )
            .unwrap();
        let record = store.portal_user_for_login(&user.email).unwrap().unwrap();
        let map = store.portal_map(&record).unwrap();
        assert_eq!(map.buildings.len(), 1);
        assert_eq!(map.buildings[0].floors.len(), 1);
        assert_eq!(map.buildings[0].floors[0].id, first_floor);
    }
}
