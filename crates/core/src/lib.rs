//! Sensor model, allowlist, and curve types shared across crates.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_roundtrips_through_json() {
        let r = Reading {
            name: "PMU tdie0".to_string(),
            source: SensorSource::Hid,
            value: 42.5,
            unit: "C".to_string(),
            timestamp_ms: 1_700_000_000_000,
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Reading = serde_json::from_str(&s).unwrap();
        assert_eq!(back.name, r.name);
        assert_eq!(back.source, r.source);
        assert_eq!(back.value, r.value);
        assert_eq!(back.unit, r.unit);
        assert_eq!(back.timestamp_ms, r.timestamp_ms);
    }

    #[test]
    fn sensor_source_serializes_uppercase() {
        assert_eq!(serde_json::to_string(&SensorSource::Hid).unwrap(), "\"HID\"");
        assert_eq!(serde_json::to_string(&SensorSource::Smc).unwrap(), "\"SMC\"");
    }
}
