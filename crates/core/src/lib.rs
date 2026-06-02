//! Sensor model, host detection, and the per-host calibration allowlist.

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
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
        // No HOME / XDG_CONFIG_HOME is a "no calibration" condition, not an error —
        // e.g. when running as a launchd daemon. Caller treats None as degraded fallback.
        let Ok(path) = config_path() else {
            return Ok(None);
        };
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

pub fn config_dir() -> Result<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or_else(|| anyhow!("neither XDG_CONFIG_HOME nor HOME is set"))?;
    Ok(base.join("msf"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("calibration.toml"))
}

pub fn profile_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("profile.toml"))
}

pub fn check_overwrite(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        return Err(anyhow!(
            "file exists; pass --force to overwrite: {}",
            path.display()
        ));
    }
    Ok(())
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

pub fn clamp_rpm(target: f64, min: f64, max: f64) -> f64 {
    if target.is_nan() {
        return min;
    }
    target.clamp(min, max)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CurvePoint {
    pub temp_c: f64,
    pub rpm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Curve {
    pub points: Vec<CurvePoint>,
}

impl Curve {
    pub fn load(path: &Path) -> Result<Self> {
        let s = fs::read_to_string(path)
            .map_err(|e| anyhow!("read curve file {}: {e}", path.display()))?;
        let c: Self = toml::from_str(&s)?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        if self.points.len() < 2 {
            return Err(anyhow!("curve must have ≥2 points (got {})", self.points.len()));
        }
        for w in self.points.windows(2) {
            if w[1].temp_c <= w[0].temp_c {
                return Err(anyhow!(
                    "points must be strictly sorted by temp_c (got {} then {})",
                    w[0].temp_c,
                    w[1].temp_c
                ));
            }
        }
        Ok(())
    }

    pub fn evaluate(&self, t: f64) -> f64 {
        let pts = &self.points;
        if t <= pts[0].temp_c {
            return pts[0].rpm;
        }
        let last = pts.len() - 1;
        if t >= pts[last].temp_c {
            return pts[last].rpm;
        }
        for w in pts.windows(2) {
            if t >= w[0].temp_c && t <= w[1].temp_c {
                let span = w[1].temp_c - w[0].temp_c;
                if span == 0.0 {
                    return w[0].rpm;
                }
                let r = (t - w[0].temp_c) / span;
                return w[0].rpm + r * (w[1].rpm - w[0].rpm);
            }
        }
        pts[last].rpm
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AsymmetricEma {
    pub rise_alpha: f64,
    pub fall_alpha: f64,
    state: Option<f64>,
}

impl AsymmetricEma {
    pub fn new(rise_alpha: f64, fall_alpha: f64) -> Self {
        Self {
            rise_alpha,
            fall_alpha,
            state: None,
        }
    }

    pub fn update(&mut self, sample: f64) -> f64 {
        if sample.is_nan() {
            return self.state.unwrap_or(sample);
        }
        match self.state {
            None => {
                self.state = Some(sample);
                sample
            }
            Some(prev) => {
                let alpha = if sample > prev {
                    self.rise_alpha
                } else {
                    self.fall_alpha
                };
                let next = prev + alpha * (sample - prev);
                self.state = Some(next);
                next
            }
        }
    }

    pub fn current(&self) -> Option<f64> {
        self.state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Quiet,
    Balanced,
    Cool,
}

impl Policy {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Quiet => "quiet",
            Self::Balanced => "balanced",
            Self::Cool => "cool",
        }
    }

    fn fractions(&self) -> &'static [(f64, f64)] {
        // (temp_c, rpm_fraction in [0.0, 1.0]). rpm = Mn + fraction * (Mx - Mn).
        match self {
            Self::Quiet => &[(50.0, 0.0), (75.0, 0.40), (90.0, 1.0)],
            Self::Balanced => &[(40.0, 0.0), (60.0, 0.35), (75.0, 0.70), (85.0, 1.0)],
            Self::Cool => &[(35.0, 0.20), (55.0, 0.55), (70.0, 0.90), (80.0, 1.0)],
        }
    }

    pub fn materialize(&self, mn: f64, mx: f64) -> Curve {
        let span = mx - mn;
        let points = self
            .fractions()
            .iter()
            .map(|&(t, f)| CurvePoint {
                temp_c: t,
                rpm: mn + f * span,
            })
            .collect();
        Curve { points }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FanSpec {
    pub index: u8,
    pub min_rpm: f64,
    pub max_rpm: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FanSelector {
    All,
    Indices(Vec<u8>),
}

pub fn parse_fan_selector(s: &str) -> Result<FanSelector> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        bail!("--fan: empty value (expected 'all' or comma-separated indices)");
    }
    if trimmed.eq_ignore_ascii_case("all") {
        return Ok(FanSelector::All);
    }
    let mut seen: HashSet<u8> = HashSet::new();
    let mut out: Vec<u8> = Vec::new();
    for tok in trimmed.split(',') {
        let tok = tok.trim();
        if tok.is_empty() {
            bail!("--fan: empty index in '{s}'");
        }
        let n: u8 = tok
            .parse()
            .map_err(|e| anyhow!("--fan: bad index '{tok}': {e}"))?;
        if seen.insert(n) {
            out.push(n);
        }
    }
    Ok(FanSelector::Indices(out))
}

impl FanSelector {
    pub fn resolve(&self, fan_count: u8) -> Result<Vec<u8>> {
        match self {
            FanSelector::All => Ok((0..fan_count).collect()),
            FanSelector::Indices(v) => {
                for &i in v {
                    if i >= fan_count {
                        bail!("--fan: index {i} out of range (fan_count={fan_count})");
                    }
                }
                Ok(v.clone())
            }
        }
    }
}

pub fn render_profile(host: &Host, fans: &[FanSpec], policy: Policy, curve: &Curve) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Generated by `msf init` on {}\n", now_iso8601()));
    s.push_str(&format!("# Host:   {} (build {})\n", host.model, host.build));
    s.push_str("# Fans:\n");
    for f in fans {
        s.push_str(&format!(
            "#   F{}: Mn={:.0}, Mx={:.0}\n",
            f.index, f.min_rpm, f.max_rpm
        ));
    }
    s.push_str(&format!("# Policy: {}\n", policy.name()));
    s.push_str("# Edit the points or regenerate with `msf init --profile <name> --force`.\n");
    s.push('\n');
    for p in &curve.points {
        s.push_str("[[points]]\n");
        s.push_str(&format!("temp_c = {}\n", p.temp_c));
        s.push_str(&format!("rpm = {}\n", p.rpm));
        s.push('\n');
    }
    s
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
    fn clamp_rpm_within_range_unchanged() {
        assert_eq!(clamp_rpm(2000.0, 1200.0, 5779.0), 2000.0);
    }

    #[test]
    fn clamp_rpm_below_min_pins_to_min() {
        assert_eq!(clamp_rpm(500.0, 1200.0, 5779.0), 1200.0);
    }

    #[test]
    fn clamp_rpm_above_max_pins_to_max() {
        assert_eq!(clamp_rpm(9999.0, 1200.0, 5779.0), 5779.0);
    }

    #[test]
    fn clamp_rpm_nan_returns_min() {
        let v = clamp_rpm(f64::NAN, 1200.0, 5779.0);
        assert_eq!(v, 1200.0);
    }

    fn curve(pts: &[(f64, f64)]) -> Curve {
        Curve {
            points: pts
                .iter()
                .map(|&(t, r)| CurvePoint { temp_c: t, rpm: r })
                .collect(),
        }
    }

    #[test]
    fn curve_validates_minimum_points() {
        assert!(curve(&[(50.0, 1200.0)]).validate().is_err());
        assert!(curve(&[]).validate().is_err());
        assert!(curve(&[(50.0, 1200.0), (80.0, 4000.0)]).validate().is_ok());
    }

    #[test]
    fn curve_rejects_unsorted_or_duplicate_points() {
        assert!(
            curve(&[(80.0, 4000.0), (50.0, 1200.0)])
                .validate()
                .is_err()
        );
        assert!(
            curve(&[(50.0, 1200.0), (50.0, 2000.0)])
                .validate()
                .is_err()
        );
    }

    #[test]
    fn curve_below_first_returns_first_rpm() {
        let c = curve(&[(50.0, 1200.0), (80.0, 4000.0)]);
        assert_eq!(c.evaluate(0.0), 1200.0);
        assert_eq!(c.evaluate(50.0), 1200.0);
    }

    #[test]
    fn curve_above_last_returns_last_rpm() {
        let c = curve(&[(50.0, 1200.0), (80.0, 4000.0)]);
        assert_eq!(c.evaluate(100.0), 4000.0);
        assert_eq!(c.evaluate(80.0), 4000.0);
    }

    #[test]
    fn curve_interpolates_linearly_between_points() {
        let c = curve(&[(50.0, 1200.0), (80.0, 4000.0)]);
        assert_eq!(c.evaluate(65.0), 2600.0);
    }

    #[test]
    fn curve_handles_multi_segment_piecewise() {
        let c = curve(&[(40.0, 1000.0), (60.0, 2000.0), (80.0, 5000.0)]);
        assert_eq!(c.evaluate(50.0), 1500.0);
        assert_eq!(c.evaluate(70.0), 3500.0);
    }

    #[test]
    fn curve_eval_same_temp_adjacent_returns_first_rpm() {
        // validate() rejects this shape, but evaluate() must not divide by zero
        // if it ever reaches the same-temp segment.
        let c = Curve {
            points: vec![
                CurvePoint { temp_c: 50.0, rpm: 1200.0 },
                CurvePoint { temp_c: 50.0, rpm: 2000.0 },
                CurvePoint { temp_c: 70.0, rpm: 3500.0 },
            ],
        };
        assert_eq!(c.evaluate(50.0), 1200.0);
    }

    #[test]
    fn curve_parser_accepts_valid_toml() {
        let s = r#"
            [[points]]
            temp_c = 40.0
            rpm = 1200.0

            [[points]]
            temp_c = 70.0
            rpm = 3500.0
        "#;
        let c: Curve = toml::from_str(s).unwrap();
        assert!(c.validate().is_ok());
        assert_eq!(c.points.len(), 2);
    }

    #[test]
    fn curve_parser_rejects_single_point_toml() {
        let s = r#"
            [[points]]
            temp_c = 40.0
            rpm = 1200.0
        "#;
        let c: Curve = toml::from_str(s).unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn curve_parser_rejects_empty_points_toml() {
        let s = "points = []";
        let c: Curve = toml::from_str(s).unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn curve_parser_rejects_unsorted_toml() {
        let s = r#"
            [[points]]
            temp_c = 70.0
            rpm = 3500.0

            [[points]]
            temp_c = 40.0
            rpm = 1200.0
        "#;
        let c: Curve = toml::from_str(s).unwrap();
        assert!(c.validate().is_err());
    }

    #[test]
    fn ema_initializes_with_first_sample() {
        let mut e = AsymmetricEma::new(0.7, 0.15);
        assert_eq!(e.update(42.0), 42.0);
        assert_eq!(e.current(), Some(42.0));
    }

    #[test]
    fn ema_uses_rise_alpha_when_increasing() {
        let mut e = AsymmetricEma::new(0.7, 0.15);
        e.update(40.0);
        let v = e.update(60.0);
        let expected = 40.0 + 0.7 * (60.0 - 40.0);
        assert!((v - expected).abs() < 1e-9);
    }

    #[test]
    fn ema_uses_fall_alpha_when_decreasing() {
        let mut e = AsymmetricEma::new(0.7, 0.15);
        e.update(60.0);
        let v = e.update(40.0);
        let expected = 60.0 + 0.15 * (40.0 - 60.0);
        assert!((v - expected).abs() < 1e-9);
    }

    #[test]
    fn ema_nan_is_ignored() {
        let mut e = AsymmetricEma::new(0.7, 0.15);
        e.update(50.0);
        let v = e.update(f64::NAN);
        assert_eq!(v, 50.0);
        assert_eq!(e.current(), Some(50.0));
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

    fn assert_policy_sane(policy: Policy) {
        let c = policy.materialize(1200.0, 5779.0);
        assert!(c.validate().is_ok(), "{} curve must validate", policy.name());
        assert!(c.points.len() >= 2);
        for p in &c.points {
            assert!(
                p.rpm >= 1200.0 && p.rpm <= 5779.0,
                "{} point ({}, {}) out of [Mn, Mx]",
                policy.name(),
                p.temp_c,
                p.rpm
            );
        }
    }

    #[test]
    fn policy_quiet_materializes_sorted_in_range() {
        assert_policy_sane(Policy::Quiet);
    }

    #[test]
    fn policy_balanced_materializes_sorted_in_range() {
        assert_policy_sane(Policy::Balanced);
    }

    #[test]
    fn policy_cool_materializes_sorted_in_range() {
        assert_policy_sane(Policy::Cool);
    }

    #[test]
    fn policy_last_point_lands_at_mx() {
        for p in [Policy::Quiet, Policy::Balanced, Policy::Cool] {
            let c = p.materialize(1200.0, 5779.0);
            let last = c.points.last().unwrap();
            assert_eq!(last.rpm, 5779.0, "{} last should be Mx", p.name());
        }
    }

    #[test]
    fn check_overwrite_blocks_existing_without_force() {
        let p = std::env::temp_dir().join(format!("msf-overwrite-test-{}", std::process::id()));
        std::fs::write(&p, "x").unwrap();
        assert!(check_overwrite(&p, false).is_err());
        assert!(check_overwrite(&p, true).is_ok());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn check_overwrite_allows_missing_path() {
        let p = std::env::temp_dir().join(format!("msf-missing-{}-{}", std::process::id(), 1));
        let _ = std::fs::remove_file(&p);
        assert!(check_overwrite(&p, false).is_ok());
        assert!(check_overwrite(&p, true).is_ok());
    }

    #[test]
    fn rendered_profile_round_trips_through_curve_parser() {
        let host = host("MacBookPro18,4", "25D2128");
        let fans = vec![
            FanSpec {
                index: 0,
                min_rpm: 1200.0,
                max_rpm: 5779.0,
            },
            FanSpec {
                index: 1,
                min_rpm: 1200.0,
                max_rpm: 6241.0,
            },
        ];
        let curve = Policy::Balanced.materialize(1200.0, 5779.0);
        let text = render_profile(&host, &fans, Policy::Balanced, &curve);
        let parsed: Curve = toml::from_str(&text).expect("rendered profile parses");
        assert!(parsed.validate().is_ok());
        assert_eq!(parsed.points, curve.points);
    }

    #[test]
    fn parse_fan_selector_all_case_insensitive() {
        assert_eq!(parse_fan_selector("all").unwrap(), FanSelector::All);
        assert_eq!(parse_fan_selector("ALL").unwrap(), FanSelector::All);
        assert_eq!(parse_fan_selector("All").unwrap(), FanSelector::All);
        assert_eq!(parse_fan_selector("  all  ").unwrap(), FanSelector::All);
    }

    #[test]
    fn parse_fan_selector_single_index() {
        assert_eq!(
            parse_fan_selector("0").unwrap(),
            FanSelector::Indices(vec![0])
        );
        assert_eq!(
            parse_fan_selector("3").unwrap(),
            FanSelector::Indices(vec![3])
        );
    }

    #[test]
    fn parse_fan_selector_csv_order_preserved() {
        assert_eq!(
            parse_fan_selector("0,1").unwrap(),
            FanSelector::Indices(vec![0, 1])
        );
        assert_eq!(
            parse_fan_selector("1,0").unwrap(),
            FanSelector::Indices(vec![1, 0])
        );
    }

    #[test]
    fn parse_fan_selector_dedupe() {
        assert_eq!(
            parse_fan_selector("0,0,1,0").unwrap(),
            FanSelector::Indices(vec![0, 1])
        );
    }

    #[test]
    fn parse_fan_selector_whitespace_tolerant() {
        assert_eq!(
            parse_fan_selector("0, 1").unwrap(),
            FanSelector::Indices(vec![0, 1])
        );
        assert_eq!(
            parse_fan_selector(" 0 , 1 ").unwrap(),
            FanSelector::Indices(vec![0, 1])
        );
    }

    #[test]
    fn parse_fan_selector_rejects_empty() {
        assert!(parse_fan_selector("").is_err());
        assert!(parse_fan_selector("   ").is_err());
        assert!(parse_fan_selector("0,").is_err());
        assert!(parse_fan_selector(",0").is_err());
    }

    #[test]
    fn parse_fan_selector_rejects_nonnumeric() {
        assert!(parse_fan_selector("abc").is_err());
        assert!(parse_fan_selector("0,a").is_err());
        assert!(parse_fan_selector("all,0").is_err());
    }

    #[test]
    fn parse_fan_selector_rejects_negative_and_overflow() {
        assert!(parse_fan_selector("-1").is_err());
        assert!(parse_fan_selector("256").is_err());
        assert!(parse_fan_selector("0,-1").is_err());
    }

    #[test]
    fn fan_selector_resolve_all_expands_to_range() {
        assert_eq!(FanSelector::All.resolve(0).unwrap(), Vec::<u8>::new());
        assert_eq!(FanSelector::All.resolve(1).unwrap(), vec![0]);
        assert_eq!(FanSelector::All.resolve(2).unwrap(), vec![0, 1]);
        assert_eq!(FanSelector::All.resolve(3).unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn fan_selector_resolve_indices_in_range_passes_through() {
        let s = FanSelector::Indices(vec![1, 0]);
        assert_eq!(s.resolve(2).unwrap(), vec![1, 0]);
        let s = FanSelector::Indices(vec![0]);
        assert_eq!(s.resolve(2).unwrap(), vec![0]);
    }

    #[test]
    fn fan_selector_resolve_indices_out_of_range_errors() {
        let s = FanSelector::Indices(vec![0, 2]);
        let err = s.resolve(2).unwrap_err().to_string();
        assert!(err.contains("out of range"));
        let s = FanSelector::Indices(vec![5]);
        assert!(s.resolve(2).is_err());
    }
}
