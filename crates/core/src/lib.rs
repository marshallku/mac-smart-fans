//! Sensor model, allowlist, and curve types shared across crates.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SensorSource {
    Hid,
    Smc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reading {
    pub name: String,
    pub source: SensorSource,
    pub value: f64,
    pub unit: String,
    pub timestamp_ms: u64,
}
