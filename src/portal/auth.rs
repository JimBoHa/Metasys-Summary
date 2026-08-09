use anyhow::{Context, Result, bail};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::models::{BuildingInput, CreateServiceRequest, FloorInput, RegionInput, TraceFeature};

pub const SESSION_COOKIE: &str = "metasys_portal_session";

pub fn hash_password(password: &str) -> Result<String> {
    validate_password(password)?;
    let salt = SaltString::encode_b64(Uuid::new_v4().as_bytes())
        .map_err(|error| anyhow::anyhow!("create password salt: {error}"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| anyhow::anyhow!("hash portal password: {error}"))
}

pub fn verify_password(password: &str, encoded_hash: &str) -> bool {
    PasswordHash::new(encoded_hash).ok().is_some_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

pub fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

pub fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

pub fn validate_password(password: &str) -> Result<()> {
    let length = password.chars().count();
    if !(12..=256).contains(&length) {
        bail!("password must contain 12 to 256 characters");
    }
    if password
        .chars()
        .all(|character| character.is_ascii_alphabetic())
        || password.chars().all(|character| character.is_ascii_digit())
    {
        bail!("password must mix character types");
    }
    Ok(())
}

pub fn validate_email(email: &str) -> Result<()> {
    let trimmed = email.trim();
    if trimmed.is_empty()
        || trimmed.len() > 254
        || trimmed.chars().any(char::is_whitespace)
        || trimmed.starts_with('@')
        || trimmed.ends_with('@')
        || trimmed.matches('@').count() != 1
    {
        bail!("enter a valid email address");
    }
    Ok(())
}

pub fn validate_user(email: &str, display_name: &str) -> Result<()> {
    validate_email(email)?;
    validate_name(display_name, "display name", 120)
}

pub fn validate_building(input: &BuildingInput) -> Result<()> {
    validate_name(&input.name, "building name", 160)
}

pub fn validate_floor(input: &FloorInput) -> Result<()> {
    if input.building_id.trim().is_empty() || input.building_id.len() > 80 {
        bail!("building is required");
    }
    validate_name(&input.name, "floor name", 160)
}

pub fn validate_region(input: &RegionInput) -> Result<()> {
    if input.floor_id.trim().is_empty() || input.floor_id.len() > 80 {
        bail!("floor is required");
    }
    validate_name(&input.name, "region name", 160)?;
    if input.polygon.len() < 3 || input.polygon.len() > 128 {
        bail!("region requires 3 to 128 boundary points");
    }
    input
        .polygon
        .iter()
        .try_for_each(|point| point.validate())?;
    if !is_hex_color(&input.color) {
        bail!("region color must use #RRGGBB format");
    }
    if input.fav_box.chars().count() > 160 {
        bail!("FAV box name is too long");
    }
    if input.metasys_object_id.chars().count() > 1_024 {
        bail!("Metasys object reference is too long");
    }
    if input.metasys_attribute_id.trim().is_empty()
        || input.metasys_attribute_id.chars().count() > 64
    {
        bail!("Metasys attribute is required");
    }
    Ok(())
}

pub fn validate_trace(trace: &[TraceFeature]) -> Result<()> {
    if trace.len() > 2_000 {
        bail!("floor-plan trace exceeds 2,000 features");
    }
    trace.iter().try_for_each(TraceFeature::validate)
}

pub fn validate_service_request(input: &CreateServiceRequest) -> Result<()> {
    if input.region_id.trim().is_empty() || input.region_id.len() > 80 {
        bail!("region is required");
    }
    validate_email(&input.contact_email)?;
    if !matches!(
        input.issue_type.as_str(),
        "too_hot"
            | "too_cold"
            | "lighting"
            | "water_leak"
            | "noise"
            | "broken_toilet"
            | "air_quality"
            | "other"
    ) {
        bail!("select a supported issue type");
    }
    if input.details.chars().count() > 2_000 {
        bail!("request details cannot exceed 2,000 characters");
    }
    Ok(())
}

pub fn validate_note(note: &str) -> Result<()> {
    let length = note.trim().chars().count();
    if !(1..=4_000).contains(&length) {
        bail!("note must contain 1 to 4,000 characters");
    }
    Ok(())
}

pub fn clean_upload_name(value: &str) -> String {
    let name = value
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("floor-plan.pdf")
        .chars()
        .filter(|character| !character.is_control())
        .take(180)
        .collect::<String>();
    if name.to_ascii_lowercase().ends_with(".pdf") {
        name
    } else {
        "floor-plan.pdf".to_owned()
    }
}

pub fn validate_plan_name(value: &str) -> Result<()> {
    validate_name(value, "floor-plan name", 160).context("invalid floor-plan name")
}

fn validate_name(value: &str, label: &str, maximum: usize) -> Result<()> {
    let length = value.trim().chars().count();
    if length == 0 || length > maximum {
        bail!("{label} must contain 1 to {maximum} characters");
    }
    Ok(())
}

fn is_hex_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_round_trip() {
        let hash = hash_password("Correct-Horse-47").unwrap();
        assert!(verify_password("Correct-Horse-47", &hash));
        assert!(!verify_password("Wrong-Horse-47", &hash));
        assert!(!hash.contains("Correct-Horse-47"));
    }

    #[test]
    fn tokens_are_random_and_hashable() {
        let first = random_token();
        let second = random_token();
        assert_ne!(first, second);
        assert_eq!(token_hash(&first).len(), 64);
    }
}
