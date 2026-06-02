use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use msf_core::{Calibration, Host, Reading, SensorSource};
use msf_smc::{FanProbe, KeyInfo};
use serde::Serialize;
use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Parser)]
#[command(
    name = "msf",
    version,
    about = "Mac Smart Fans — Apple Silicon thermal monitor and fan control"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print sensor + fan readings (read-only, no sudo)
    Monitor {
        #[arg(long)]
        json: bool,

        #[arg(long, default_value_t = 1.0)]
        interval_secs: f64,

        #[arg(long, default_value_t = 0)]
        ticks: u32,

        /// Restrict output to sensors in the calibration allowlist
        #[arg(long)]
        selected: bool,
    },
    /// Manage the per-host sensor allowlist
    Calibrate {
        #[command(subcommand)]
        action: CalibrateAction,
    },
    /// Read-only SMC capability discovery (no writes)
    Probe {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum CalibrateAction {
    /// Add a sensor name to the allowlist (idempotent)
    Add { name: String },
    /// Remove a sensor from the allowlist (idempotent)
    Remove { name: String },
    /// Show the current allowlist and host key
    List,
}

fn main() -> Result<()> {
    // Rust ignores SIGPIPE by default; restore the Unix CLI convention of
    // terminating cleanly when downstream (e.g. `| head`) closes the pipe.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Monitor {
            json,
            interval_secs,
            ticks,
            selected,
        } => run_monitor(json, interval_secs, ticks, selected),
        Command::Calibrate { action } => run_calibrate(action),
        Command::Probe { json } => run_probe(json),
    }
}

#[derive(Serialize)]
struct ProbeSummary {
    model: String,
    build: String,
    fan_count: u8,
    ftst: FtstField,
    fans: Vec<FanProbe>,
    controllable: bool,
    not_controllable_reason: Option<String>,
}

#[derive(Serialize)]
struct FtstField {
    present: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data_attributes: Option<u8>,
}

impl From<Option<KeyInfo>> for FtstField {
    fn from(k: Option<KeyInfo>) -> Self {
        match k {
            None => Self {
                present: false,
                data_type: None,
                data_size: None,
                data_attributes: None,
            },
            Some(k) => Self {
                present: true,
                data_type: Some(k.data_type),
                data_size: Some(k.data_size),
                data_attributes: Some(k.data_attributes),
            },
        }
    }
}

fn run_probe(json: bool) -> Result<()> {
    let host = Host::detect()?;
    let probe = msf_smc::probe()?;
    let reason = probe.not_controllable_reason();
    let summary = ProbeSummary {
        model: host.model,
        build: host.build,
        fan_count: probe.fan_count,
        ftst: probe.ftst.into(),
        fans: probe.fans,
        controllable: reason.is_none(),
        not_controllable_reason: reason,
    };

    if json {
        println!("{}", serde_json::to_string(&summary)?);
    } else {
        print_probe(&summary);
    }
    Ok(())
}

fn print_probe(s: &ProbeSummary) {
    println!("model:       {}", s.model);
    println!("build:       {}", s.build);
    println!("fan_count:   {}", s.fan_count);
    if s.ftst.present {
        println!(
            "ftst:        present (type={:?}, size={}, attrs=0x{:02x})",
            s.ftst.data_type.as_deref().unwrap_or("?"),
            s.ftst.data_size.unwrap_or(0),
            s.ftst.data_attributes.unwrap_or(0),
        );
    } else {
        println!("ftst:        ABSENT");
    }
    for fan in &s.fans {
        println!();
        println!("fan #{}:", fan.index);
        let casing = match fan.mode_key_casing {
            Some(msf_smc::ModeKeyCasing::Upper) => "F{N}Md (uppercase)",
            Some(msf_smc::ModeKeyCasing::Lower) => "F{N}md (lowercase)",
            None => "NONE",
        };
        println!("  mode key casing: {casing}");
        print_key("Md", &fan.md);
        print_key("Tg", &fan.tg);
        print_key("Mn", &fan.mn);
        print_key("Mx", &fan.mx);
    }
    println!();
    println!(
        "controllable: {}{}",
        s.controllable,
        s.not_controllable_reason
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default(),
    );
}

fn print_key(label: &str, k: &Option<KeyInfo>) {
    match k {
        Some(k) => println!(
            "  {label}: type={:?} size={} attrs=0x{:02x}",
            k.data_type, k.data_size, k.data_attributes
        ),
        None => println!("  {label}: ABSENT"),
    }
}

fn run_calibrate(action: CalibrateAction) -> Result<()> {
    let host = Host::detect()?;
    let mut cal = Calibration::load()?.unwrap_or_else(|| Calibration::for_host(&host));

    match action {
        CalibrateAction::Add { name } => {
            if cal.add(&name) {
                cal.save()?;
                eprintln!("added: {name}");
            } else {
                eprintln!("already present: {name}");
            }
        }
        CalibrateAction::Remove { name } => {
            if cal.remove(&name) {
                cal.save()?;
                eprintln!("removed: {name}");
            } else {
                eprintln!("not present: {name}");
            }
        }
        CalibrateAction::List => {
            println!("model:       {}", cal.model);
            println!("build:       {}", cal.build);
            println!("recorded_at: {}", cal.recorded_at);
            println!("sensors ({}):", cal.sensors.len());
            for (name, entry) in &cal.sensors {
                println!("  - {name} (added {})", entry.added_at);
            }
            if !cal.matches_host(&host) {
                eprintln!(
                    "warning: calibration host mismatch (current: {}/{}, recorded: {}/{})",
                    host.model, host.build, cal.model, cal.build
                );
            }
        }
    }
    Ok(())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn collect_readings() -> Vec<Reading> {
    let mut out = Vec::new();
    let ts = now_ms();

    match msf_hid::read_all() {
        Ok(hids) => {
            for s in hids {
                out.push(Reading {
                    name: s.name,
                    source: SensorSource::Hid,
                    value: s.celsius,
                    unit: "C".to_string(),
                    timestamp_ms: ts,
                });
            }
        }
        Err(e) => tracing::warn!("hid read failed: {e:#}"),
    }

    match msf_smc::read_fans() {
        Ok(fans) => {
            for f in fans {
                out.push(Reading {
                    name: format!("F{}Ac", f.index),
                    source: SensorSource::Smc,
                    value: f.actual_rpm,
                    unit: "rpm".to_string(),
                    timestamp_ms: ts,
                });
                out.push(Reading {
                    name: format!("F{}Mn", f.index),
                    source: SensorSource::Smc,
                    value: f.min_rpm,
                    unit: "rpm".to_string(),
                    timestamp_ms: ts,
                });
                out.push(Reading {
                    name: format!("F{}Mx", f.index),
                    source: SensorSource::Smc,
                    value: f.max_rpm,
                    unit: "rpm".to_string(),
                    timestamp_ms: ts,
                });
            }
        }
        Err(e) => tracing::warn!("smc read failed: {e:#}"),
    }

    out
}

fn run_monitor(json: bool, interval_secs: f64, ticks: u32, selected: bool) -> Result<()> {
    let filter: Option<HashSet<String>> = if selected {
        match Calibration::load()? {
            None => bail!("no calibration found; run `msf calibrate add <sensor>` first"),
            Some(c) => Some(c.sensors.into_keys().collect()),
        }
    } else {
        None
    };

    let interval = Duration::from_secs_f64(interval_secs.max(0.05));
    let mut count = 0u32;
    loop {
        let readings = collect_readings();
        emit(&readings, &filter, json)?;
        count += 1;
        if ticks > 0 && count >= ticks {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

fn emit(readings: &[Reading], filter: &Option<HashSet<String>>, json: bool) -> Result<()> {
    let pass = |r: &Reading| filter.as_ref().is_none_or(|allow| allow.contains(&r.name));

    if !json {
        println!("{:<44} {:>4} {:>10} {:>5}", "name", "src", "value", "unit");
    }
    for r in readings {
        if !pass(r) {
            continue;
        }
        if json {
            println!("{}", serde_json::to_string(r)?);
        } else {
            print_row(r);
        }
    }
    if !json {
        println!();
    }
    Ok(())
}

fn print_row(r: &Reading) {
    let src = match r.source {
        SensorSource::Hid => "HID",
        SensorSource::Smc => "SMC",
    };
    println!(
        "{:<44} {:>4} {:>10.2} {:>5}",
        truncate(&r.name, 44),
        src,
        r.value,
        r.unit
    );
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
