use std::collections::BTreeSet;

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const MAX_GROUPS: usize = 100;
const MAX_EQUIPMENT: usize = 2_000;
const MAX_POINTS: usize = 20_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EquipmentInventory {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub root_name: String,
    pub captured_at: DateTime<Utc>,
    pub source_summary: String,
    #[serde(default)]
    pub notes: Vec<String>,
    pub groups: Vec<EquipmentGroup>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EquipmentGroup {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub equipment: Vec<EquipmentItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EquipmentItem {
    pub name: String,
    pub equipment_type: String,
    pub variant: String,
    pub protocol: String,
    #[serde(default)]
    pub network_name: String,
    pub mac_address: Option<u16>,
    pub device_instance: Option<u32>,
    #[serde(default)]
    pub object_reference: String,
    pub discovery_status: String,
    pub source: String,
    pub points: Vec<EquipmentPoint>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EquipmentPoint {
    pub name: String,
    #[serde(default)]
    pub reference: String,
    pub category: String,
    pub unit: Option<String>,
    pub historian_point_slice_id: Option<i32>,
    pub source: String,
}

impl EquipmentInventory {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!(
                "unsupported equipment inventory schema version {}",
                self.schema_version
            );
        }
        validate_text("root name", &self.root_name, 160)?;
        validate_text("source summary", &self.source_summary, 1_000)?;
        if self.groups.is_empty() || self.groups.len() > MAX_GROUPS {
            bail!("equipment inventory must contain between 1 and {MAX_GROUPS} groups");
        }
        for note in &self.notes {
            validate_text("inventory note", note, 1_000)?;
        }

        let mut group_names = BTreeSet::new();
        let mut equipment_count = 0;
        let mut point_count = 0;
        for group in &self.groups {
            validate_text("group name", &group.name, 160)?;
            validate_optional_text("group description", &group.description, 1_000)?;
            if !group_names.insert(group.name.to_ascii_lowercase()) {
                bail!("duplicate equipment group {}", group.name);
            }
            let mut equipment_names = BTreeSet::new();
            for equipment in &group.equipment {
                equipment_count += 1;
                validate_text("equipment name", &equipment.name, 160)?;
                validate_text("equipment type", &equipment.equipment_type, 120)?;
                validate_text("equipment variant", &equipment.variant, 160)?;
                validate_text("equipment protocol", &equipment.protocol, 120)?;
                validate_optional_text("network name", &equipment.network_name, 160)?;
                validate_optional_text("object reference", &equipment.object_reference, 512)?;
                validate_text("discovery status", &equipment.discovery_status, 300)?;
                validate_text("equipment source", &equipment.source, 300)?;
                if !equipment_names.insert(equipment.name.to_ascii_lowercase()) {
                    bail!(
                        "duplicate equipment {} in group {}",
                        equipment.name,
                        group.name
                    );
                }
                if equipment.mac_address.is_some_and(|address| address > 127) {
                    bail!("MS/TP MAC address must be between 0 and 127");
                }
                if equipment
                    .device_instance
                    .is_some_and(|instance| instance > 4_194_303)
                {
                    bail!("BACnet device instance must be between 0 and 4194303");
                }
                let mut point_names = BTreeSet::new();
                for point in &equipment.points {
                    point_count += 1;
                    validate_text("point name", &point.name, 200)?;
                    validate_optional_text("point reference", &point.reference, 700)?;
                    validate_text("point category", &point.category, 120)?;
                    validate_optional_text(
                        "point unit",
                        point.unit.as_deref().unwrap_or_default(),
                        80,
                    )?;
                    validate_text("point source", &point.source, 300)?;
                    if point
                        .historian_point_slice_id
                        .is_some_and(|identifier| identifier <= 0)
                    {
                        bail!("historian point slice identifiers must be positive");
                    }
                    if !point_names.insert(point.name.to_ascii_lowercase()) {
                        bail!(
                            "duplicate point {} on equipment {}",
                            point.name,
                            equipment.name
                        );
                    }
                }
            }
        }
        if equipment_count == 0 || equipment_count > MAX_EQUIPMENT {
            bail!("equipment inventory must contain between 1 and {MAX_EQUIPMENT} equipment");
        }
        if point_count == 0 || point_count > MAX_POINTS {
            bail!("equipment inventory must contain between 1 and {MAX_POINTS} points");
        }
        Ok(())
    }

    pub fn equipment_count(&self) -> usize {
        self.groups.iter().map(|group| group.equipment.len()).sum()
    }

    pub fn point_count(&self) -> usize {
        self.groups
            .iter()
            .flat_map(|group| &group.equipment)
            .map(|equipment| equipment.points.len())
            .sum()
    }
}

fn default_schema_version() -> u32 {
    1
}

fn validate_text(label: &str, value: &str, maximum: usize) -> Result<()> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{label} cannot be empty");
    }
    if value.chars().count() > maximum {
        bail!("{label} cannot exceed {maximum} characters");
    }
    Ok(())
}

fn validate_optional_text(label: &str, value: &str, maximum: usize) -> Result<()> {
    if value.chars().count() > maximum {
        bail!("{label} cannot exceed {maximum} characters");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{EquipmentGroup, EquipmentInventory, EquipmentItem, EquipmentPoint};

    fn fixture() -> EquipmentInventory {
        EquipmentInventory {
            schema_version: 1,
            root_name: "B Mod".to_owned(),
            captured_at: Utc::now(),
            source_summary: "Passive discovery".to_owned(),
            notes: Vec::new(),
            groups: vec![EquipmentGroup {
                name: "VAVs".to_owned(),
                description: String::new(),
                equipment: vec![EquipmentItem {
                    name: "TB2-101".to_owned(),
                    equipment_type: "terminalBox".to_owned(),
                    variant: "fanPoweredHeating".to_owned(),
                    protocol: "BACnet MS/TP".to_owned(),
                    network_name: "B2-NAE / FC-1".to_owned(),
                    mac_address: Some(4),
                    device_instance: Some(1_049_854),
                    object_reference: "BMSServer:B2-NAE/FC-1.021004FE".to_owned(),
                    discovery_status: "Active".to_owned(),
                    source: "test".to_owned(),
                    points: vec![EquipmentPoint {
                        name: "ZN-T".to_owned(),
                        reference: "BMSServer:B2-NAE/FC-1.021004FE.ZN-T".to_owned(),
                        category: "temperature".to_owned(),
                        unit: Some("degF".to_owned()),
                        historian_point_slice_id: Some(1),
                        source: "historian".to_owned(),
                    }],
                }],
            }],
        }
    }

    #[test]
    fn validates_inventory() {
        let inventory = fixture();
        inventory.validate().unwrap();
        assert_eq!(inventory.equipment_count(), 1);
        assert_eq!(inventory.point_count(), 1);
    }

    #[test]
    fn rejects_duplicate_points() {
        let mut inventory = fixture();
        let point = inventory.groups[0].equipment[0].points[0].clone();
        inventory.groups[0].equipment[0].points.push(point);
        assert!(inventory.validate().is_err());
    }
}
