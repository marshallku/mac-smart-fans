//! Sensor model, host detection, and the per-host calibration allowlist.

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub model: String,
    pub build: String,
}

impl Host {
    pub fn detect() -> Result<Self> {
        Ok(Self {
            model: shell_capture("sysctl", &["-n", "hw.model"])?,
            build: shell_capture("sw_vers", &["-buildVersion"])?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Calibration {
    pub model: String,
    pub build: String,
    pub recorded_at: String,
    #[serde(default)]
    pub sensors: BTreeMap<String, SensorEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SensorEntry {
    pub added_at: String,
}

impl Calibration {
    pub fn for_host(host: &Host) -> Self {
        Self {
            model: host.model.clone(),
            build: host.build.clone(),
            recorded_at: now_iso8601(),
            sensors: BTreeMap::new(),
        }
    }

    pub fn load() -> Result<Option<Self>> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(None);
        }
        let s = fs::read_to_string(&path)?;
        Ok(Some(toml::from_str(&s)?))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn add(&mut self, name: &str) -> bool {
        if self.sensors.contains_key(name) {
            return false;
        }
        self.sensors.insert(
            name.to_string(),
            SensorEntry {
                added_at: now_iso8601(),
            },
        );
        true
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.sensors.remove(name).is_some()
    }

    pub fn matches_host(&self, host: &Host) -> bool {
        self.model == host.model && self.build == host.build
    }
}

pub fn config_path() -> Result<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(base.join("msf").join("calibration.toml"))
}

fn shell_capture(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| anyhow!("{cmd} not runnable: {e}"))?;
    if !out.status.success() {
        return Err(anyhow!("{cmd} exited with status {}", out.status));
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

fn now_iso8601() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
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

    fn host(m: &str, b: &str) -> Host {
        Host {
            model: m.to_string(),
            build: b.to_string(),
        }
    }

    #[test]
    fn calibration_add_is_idempotent_and_sorts() {
        let mut c = Calibration::for_host(&host("M", "B"));
        assert!(c.add("PMU tdie1"));
        assert!(c.add("PMU tdie0"));
        assert!(!c.add("PMU tdie0"));
        let names: Vec<&String> = c.sensors.keys().collect();
        assert_eq!(names, vec!["PMU tdie0", "PMU tdie1"]);
    }

    #[test]
    fn calibration_remove_idempotent() {
        let mut c = Calibration::for_host(&host("M", "B"));
        c.add("PMU tdie0");
        assert!(c.remove("PMU tdie0"));
        assert!(!c.remove("PMU tdie0"));
        assert!(c.sensors.is_empty());
    }

    #[test]
    fn calibration_toml_roundtrip() {
        let mut c = Calibration::for_host(&host("MacBookPro18,4", "25D80"));
        c.add("PMU tdie0");
        c.add("PMU tdie1");
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Calibration = toml::from_str(&s).unwrap();
        assert_eq!(c, back);
    }

    #[test]
    fn matches_host_strict() {
        let c = Calibration::for_host(&host("M", "B"));
        assert!(c.matches_host(&host("M", "B")));
        assert!(!c.matches_host(&host("M", "B2")));
        assert!(!c.matches_host(&host("M2", "B")));
    }

    #[test]
    fn now_iso8601_format() {
        let s = now_iso8601();
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), 20);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
    }
}
