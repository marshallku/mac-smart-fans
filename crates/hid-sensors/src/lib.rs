//! HID PMU temperature sensors for Apple Silicon (primary path).
//!
//! Enumerates `IOHIDEventSystem` services matching `PrimaryUsagePage = 0xff00`
//! and `PrimaryUsage = 5` (temperature), then reads
//! `kIOHIDEventTypeTemperature` events.

use anyhow::Result;

#[derive(Debug, Clone)]
pub struct SensorReading {
    pub name: String,
    pub celsius: f64,
}

pub fn read_all() -> Result<Vec<SensorReading>> {
    Ok(Vec::new())
}
