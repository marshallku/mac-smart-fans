use anyhow::{Result, anyhow, bail};
use clap::{Parser, Subcommand, ValueEnum};
use msf_core::{
    AsymmetricEma, Calibration, Curve, FanSpec, Host, Policy, Reading, SensorSource,
    check_overwrite, clamp_rpm, profile_path, render_profile,
};
use msf_smc::{FanProbe, KeyInfo, ManualFanSession};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// Put a fan into manual mode at <rpm> for a bounded duration (root required)
    Set {
        /// Fan index (0..fan_count from probe)
        fan: u8,
        /// Target RPM
        rpm: f64,
        /// How long to hold the manual setpoint before auto-restore
        #[arg(long, default_value_t = 10)]
        duration_secs: u64,
        /// Skip clamping to [F{N}Mn, F{N}Mx]
        #[arg(long)]
        no_clamp: bool,
        /// Emit one JSON object per state transition
        #[arg(long)]
        json: bool,
    },
    /// Generate a starter curve profile TOML from live fan spec (no root needed)
    Init {
        /// Output path; defaults to $XDG_CONFIG_HOME/msf/profile.toml (or ~/.config/msf/profile.toml)
        #[arg(long)]
        output: Option<PathBuf>,
        /// Policy: shape of the temp→RPM curve
        #[arg(long, value_enum, default_value_t = PolicyArg::Balanced)]
        profile: PolicyArg,
        /// Overwrite existing file
        #[arg(long)]
        force: bool,
    },
    /// Run a temperature-driven curve loop until TTL or Ctrl+C (root required)
    Run {
        /// Path to TOML curve config
        #[arg(long)]
        curve: PathBuf,
        /// Fan index to control
        #[arg(long, default_value_t = 0)]
        fan: u8,
        /// How long to run before auto-restore
        #[arg(long, default_value_t = 60)]
        duration_secs: u64,
        /// Emit one JSON object per state transition + per tick
        #[arg(long)]
        json: bool,
    },
    /// Install msf as a launchd LaunchDaemon that runs the curve loop persistently (root required)
    Install {
        /// Curve TOML to bind into the plist
        #[arg(long)]
        curve: PathBuf,
        /// Fan index for the daemon to control
        #[arg(long, default_value_t = 0)]
        fan: u8,
        /// Override binary path (default: current_exe() with /usr/local/bin/msf fallback)
        #[arg(long)]
        binary: Option<PathBuf>,
        /// Overwrite an existing plist
        #[arg(long)]
        force: bool,
    },
    /// Uninstall the launchd daemon (root required)
    Uninstall,
    /// Report whether the daemon plist is installed and loaded (no root)
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum PolicyArg {
    Quiet,
    Balanced,
    Cool,
}

impl From<PolicyArg> for Policy {
    fn from(p: PolicyArg) -> Self {
        match p {
            PolicyArg::Quiet => Policy::Quiet,
            PolicyArg::Balanced => Policy::Balanced,
            PolicyArg::Cool => Policy::Cool,
        }
    }
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
        Command::Init {
            output,
            profile,
            force,
        } => run_init(output, profile.into(), force),
        Command::Probe { json } => run_probe(json),
        Command::Set {
            fan,
            rpm,
            duration_secs,
            no_clamp,
            json,
        } => run_set(fan, rpm, duration_secs, no_clamp, json),
        Command::Run {
            curve,
            fan,
            duration_secs,
            json,
        } => run_curve(curve, fan, duration_secs, json),
        Command::Install {
            curve,
            fan,
            binary,
            force,
        } => run_install(curve, fan, binary, force),
        Command::Uninstall => run_uninstall(),
        Command::Status { json } => run_status(json),
    }
}

const DAEMON_LABEL: &str = "im.toss.mac-smart-fans";
const PLIST_PATH: &str = "/Library/LaunchDaemons/im.toss.mac-smart-fans.plist";
const LOG_DIR: &str = "/var/log/msf";
const DAEMON_DURATION_SECS: u64 = 31_536_000; // one year

struct PlistConfig {
    binary: PathBuf,
    curve: PathBuf,
    fan: u8,
}

fn render_plist(cfg: &PlistConfig) -> Result<Vec<u8>> {
    let mut dict = plist::Dictionary::new();
    dict.insert("Label".into(), plist::Value::String(DAEMON_LABEL.into()));
    dict.insert(
        "ProgramArguments".into(),
        plist::Value::Array(vec![
            cfg.binary.display().to_string().into(),
            "run".into(),
            "--curve".into(),
            cfg.curve.display().to_string().into(),
            "--fan".into(),
            cfg.fan.to_string().into(),
            "--duration-secs".into(),
            DAEMON_DURATION_SECS.to_string().into(),
        ]),
    );
    dict.insert("RunAtLoad".into(), plist::Value::Boolean(true));
    dict.insert("KeepAlive".into(), plist::Value::Boolean(true));
    dict.insert(
        "StandardOutPath".into(),
        plist::Value::String(format!("{LOG_DIR}/stdout.log")),
    );
    dict.insert(
        "StandardErrorPath".into(),
        plist::Value::String(format!("{LOG_DIR}/stderr.log")),
    );
    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &plist::Value::Dictionary(dict))?;
    Ok(buf)
}

fn resolve_binary_with<F: Fn() -> Option<PathBuf>>(arg: Option<PathBuf>, current: F) -> PathBuf {
    arg.or_else(current)
        .unwrap_or_else(|| PathBuf::from("/usr/local/bin/msf"))
}

fn resolve_binary(arg: Option<PathBuf>) -> PathBuf {
    resolve_binary_with(arg, || std::env::current_exe().ok())
}

fn parse_curve_path_from_plist(path: &Path) -> Result<PathBuf> {
    let value = plist::Value::from_file(path)?;
    let dict = value
        .as_dictionary()
        .ok_or_else(|| anyhow!("plist root is not a dictionary"))?;
    let args = dict
        .get("ProgramArguments")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("plist missing ProgramArguments array"))?;
    let mut iter = args.iter();
    while let Some(v) = iter.next() {
        if v.as_string() == Some("--curve")
            && let Some(next) = iter.next().and_then(|v| v.as_string())
        {
            return Ok(PathBuf::from(next));
        }
    }
    Err(anyhow!("plist ProgramArguments has no --curve flag"))
}

fn run_install(
    curve: PathBuf,
    fan: u8,
    binary: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("msf install requires root (writes /Library/LaunchDaemons/). Re-run with sudo.");
        std::process::exit(2);
    }

    let plist_path = PathBuf::from(PLIST_PATH);
    check_overwrite(&plist_path, force)?;

    let abs_curve = curve
        .canonicalize()
        .map_err(|e| anyhow!("curve path: {e} ({})", curve.display()))?;
    let binary_path = resolve_binary(binary);

    let cfg = PlistConfig {
        binary: binary_path,
        curve: abs_curve,
        fan,
    };
    let body = render_plist(&cfg)?;

    std::fs::create_dir_all(LOG_DIR)?;
    use std::os::unix::fs::PermissionsExt;
    let mut log_perms = std::fs::metadata(LOG_DIR)?.permissions();
    log_perms.set_mode(0o755);
    std::fs::set_permissions(LOG_DIR, log_perms)?;
    // explicit chown root:wheel — covers pre-existing dirs with wrong owner and the
    // egid != 0 edge case codex round 2 flagged.
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c = CString::new(std::path::Path::new(LOG_DIR).as_os_str().as_bytes())
            .map_err(|e| anyhow!("CString: {e}"))?;
        let rc = unsafe { libc::chown(c.as_ptr(), 0, 0) };
        if rc != 0 {
            return Err(anyhow!(
                "chown {LOG_DIR}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    if plist_path.exists() {
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &format!("system/{DAEMON_LABEL}")])
            .status();
    }

    std::fs::write(&plist_path, body)?;
    let mut perms = std::fs::metadata(&plist_path)?.permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&plist_path, perms)?;

    let bootstrap_status = std::process::Command::new("launchctl")
        .args(["bootstrap", "system", PLIST_PATH])
        .status()?;
    if !bootstrap_status.success() {
        let load_status = std::process::Command::new("launchctl")
            .args(["load", PLIST_PATH])
            .status()?;
        if !load_status.success() {
            bail!("both `launchctl bootstrap` and `launchctl load` failed");
        }
    }

    eprintln!("installed and loaded: {}", plist_path.display());
    eprintln!("logs:                  {LOG_DIR}/{{stdout,stderr}}.log");
    eprintln!("check:                 msf status");
    Ok(())
}

fn run_uninstall() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!(
            "msf uninstall requires root (removes /Library/LaunchDaemons/). Re-run with sudo."
        );
        std::process::exit(2);
    }
    let plist_path = PathBuf::from(PLIST_PATH);

    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("system/{DAEMON_LABEL}")])
        .stderr(std::process::Stdio::null())
        .status();
    let _ = std::process::Command::new("launchctl")
        .args(["unload", PLIST_PATH])
        .stderr(std::process::Stdio::null())
        .status();

    if plist_path.exists() {
        std::fs::remove_file(&plist_path)?;
        eprintln!("removed: {}", plist_path.display());
    } else {
        eprintln!("(already absent: {})", plist_path.display());
    }
    Ok(())
}

#[derive(Serialize)]
struct StatusReport {
    plist_present: bool,
    loaded: bool,
    curve_path: Option<String>,
}

fn run_status(json: bool) -> Result<()> {
    let plist_path = PathBuf::from(PLIST_PATH);
    let plist_present = plist_path.exists();
    let curve_path = if plist_present {
        parse_curve_path_from_plist(&plist_path)
            .ok()
            .map(|p| p.display().to_string())
    } else {
        None
    };
    let loaded = std::process::Command::new("launchctl")
        .args(["print", &format!("system/{DAEMON_LABEL}")])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let report = StatusReport {
        plist_present,
        loaded,
        curve_path,
    };

    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!(
            "plist:  {} ({})",
            PLIST_PATH,
            if report.plist_present {
                "present"
            } else {
                "absent"
            }
        );
        println!("loaded: {}", report.loaded);
        if let Some(c) = &report.curve_path {
            println!("curve:  {c}");
        }
    }
    Ok(())
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum SetEvent<'a> {
    Armed {
        fan: u8,
        mode_key_type: &'a str,
    },
    TargetWritten {
        fan: u8,
        target_rpm: f64,
    },
    Tick {
        t: u64,
        max_temp_c: f64,
        target_rpm: f64,
    },
    Trip {
        sensor: &'a str,
        celsius: f64,
    },
    RestoreStarted,
    RestoreDone,
    RestoreFailed,
    Exit {
        code: i32,
        reason: &'a str,
    },
}

fn emit_event(json: bool, ev: &SetEvent<'_>) {
    if json {
        if let Ok(s) = serde_json::to_string(ev) {
            println!("{s}");
        }
    } else {
        eprintln!("{ev:?}");
    }
}

impl std::fmt::Debug for SetEvent<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetEvent::Armed { fan, mode_key_type } => {
                write!(f, "armed: fan={fan} mode_key_type={mode_key_type}")
            }
            SetEvent::TargetWritten { fan, target_rpm } => {
                write!(f, "target_written: fan={fan} target_rpm={target_rpm}")
            }
            SetEvent::Tick {
                t,
                max_temp_c,
                target_rpm,
            } => {
                write!(
                    f,
                    "tick: t={t} max_temp_c={max_temp_c:.2} target_rpm={target_rpm:.0}"
                )
            }
            SetEvent::Trip { sensor, celsius } => {
                write!(f, "TRIP: {sensor}={celsius:.1}°C")
            }
            SetEvent::RestoreStarted => write!(f, "restore_started"),
            SetEvent::RestoreDone => write!(f, "restore_done"),
            SetEvent::RestoreFailed => write!(f, "RESTORE-FAILED"),
            SetEvent::Exit { code, reason } => write!(f, "exit: code={code} reason={reason:?}"),
        }
    }
}

const TRIP_THRESHOLD_C: f64 = 90.0;

fn run_set(fan: u8, rpm: f64, duration_secs: u64, no_clamp: bool, json: bool) -> Result<()> {
    if !rpm.is_finite() {
        bail!("rpm must be a finite number (got {rpm})");
    }
    if unsafe { libc::geteuid() } != 0 {
        eprintln!(
            "msf set requires root (writes SMC keys). Re-run with: sudo msf set {fan} {rpm}"
        );
        std::process::exit(2);
    }

    let probe = msf_smc::probe()?;
    if fan >= probe.fan_count {
        bail!(
            "fan {fan} out of range (probe reports fan_count={})",
            probe.fan_count
        );
    }
    let fp = &probe.fans[fan as usize];
    let (Some(md_info), Some(tg_info), Some(casing)) = (&fp.md, &fp.tg, fp.mode_key_casing) else {
        bail!(
            "fan {fan} not controllable: Md/Tg/casing missing in probe"
        );
    };

    let fan_readings = msf_smc::read_fans()?;
    let live = fan_readings
        .iter()
        .find(|f| f.index == fan)
        .ok_or_else(|| anyhow::anyhow!("fan {fan} not found in read_fans"))?;
    let target = if no_clamp {
        if rpm < live.min_rpm || rpm > live.max_rpm {
            eprintln!(
                "warning: --no-clamp; target {rpm} outside spec [{}, {}]",
                live.min_rpm, live.max_rpm
            );
        }
        rpm
    } else {
        let clamped = clamp_rpm(rpm, live.min_rpm, live.max_rpm);
        if (clamped - rpm).abs() > 0.01 {
            eprintln!("clamped target from {rpm} to {clamped} RPM");
        }
        clamped
    };

    let host = Host::detect()?;
    let allowlist: Option<HashSet<String>> = match Calibration::load()? {
        Some(c) if c.matches_host(&host) => Some(c.sensors.into_keys().collect()),
        Some(_) => {
            eprintln!(
                "warning: calibration host mismatch; degraded trip — scanning all HID sensors"
            );
            None
        }
        None => {
            eprintln!("warning: no calibration; degraded trip — scanning all HID sensors");
            None
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))?;
    }

    let mut session = ManualFanSession::arm(fan, casing, md_info)?;
    emit_event(
        json,
        &SetEvent::Armed {
            fan,
            mode_key_type: &md_info.data_type,
        },
    );

    session.write_target(tg_info, target)?;
    emit_event(
        json,
        &SetEvent::TargetWritten {
            fan,
            target_rpm: target,
        },
    );

    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let mut tripped: Option<(String, f64)> = None;
    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        if let Ok(temps) = msf_hid::read_all() {
            for s in &temps {
                let in_scope = allowlist.as_ref().is_none_or(|a| a.contains(&s.name));
                if in_scope && s.celsius >= TRIP_THRESHOLD_C {
                    tripped = Some((s.name.clone(), s.celsius));
                    break;
                }
            }
        }
        if tripped.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1000));
    }

    if let Some((name, c)) = &tripped {
        emit_event(
            json,
            &SetEvent::Trip {
                sensor: name,
                celsius: *c,
            },
        );
    }

    emit_event(json, &SetEvent::RestoreStarted);
    let restore_ok = session.restore().unwrap_or(false);
    if restore_ok {
        emit_event(json, &SetEvent::RestoreDone);
    } else {
        emit_event(json, &SetEvent::RestoreFailed);
    }

    let (code, reason): (i32, &str) = if !restore_ok {
        (3, "restore_failed")
    } else if tripped.is_some() {
        (4, "sensor_trip")
    } else if stop.load(Ordering::SeqCst) {
        (0, "sigint")
    } else {
        (0, "ttl")
    };
    emit_event(json, &SetEvent::Exit { code, reason });
    std::process::exit(code);
}

fn run_init(output: Option<PathBuf>, policy: Policy, force: bool) -> Result<()> {
    let target_path = match output {
        Some(p) => p,
        None => profile_path()?,
    };

    check_overwrite(&target_path, force)?;

    let host = Host::detect()?;
    let probe = msf_smc::probe()?;
    if let Some(reason) = probe.not_controllable_reason() {
        eprintln!("warning: host probed as not-controllable: {reason}");
    }
    let fans = msf_smc::read_fans()?;
    if fans.is_empty() {
        bail!("read_fans returned 0 fans");
    }
    if fans.len() != probe.fan_count as usize {
        eprintln!(
            "warning: probe fan_count={} disagrees with read_fans count={}",
            probe.fan_count,
            fans.len()
        );
    }

    let primary = &fans[0];
    let curve = policy.materialize(primary.min_rpm, primary.max_rpm);

    let specs: Vec<FanSpec> = fans
        .iter()
        .map(|f| FanSpec {
            index: f.index,
            min_rpm: f.min_rpm,
            max_rpm: f.max_rpm,
        })
        .collect();

    let body = render_profile(&host, &specs, policy, &curve);

    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target_path, body)?;

    eprintln!("wrote {} ({} policy)", target_path.display(), policy.name());
    eprintln!(
        "next:  sudo msf run --curve {} --fan 0 --duration-secs 600",
        target_path.display()
    );
    Ok(())
}

fn run_curve(curve_path: PathBuf, fan: u8, duration_secs: u64, json: bool) -> Result<()> {
    let is_root = unsafe { libc::geteuid() } == 0;

    let curve = Curve::load(&curve_path)?;

    let probe = msf_smc::probe()?;
    if fan >= probe.fan_count {
        bail!(
            "fan {fan} out of range (probe reports fan_count={})",
            probe.fan_count
        );
    }
    let fp = &probe.fans[fan as usize];
    let (Some(md_info), Some(tg_info), Some(casing)) = (&fp.md, &fp.tg, fp.mode_key_casing) else {
        bail!("fan {fan} not controllable: Md/Tg/casing missing in probe");
    };

    let fan_readings = msf_smc::read_fans()?;
    let live = fan_readings
        .iter()
        .find(|f| f.index == fan)
        .ok_or_else(|| anyhow::anyhow!("fan {fan} not found in read_fans"))?;

    if !is_root {
        eprintln!(
            "smoke OK: curve has {} points, fan {} controllable (Mn={:.0}/Mx={:.0}). Re-run with sudo to activate.",
            curve.points.len(),
            fan,
            live.min_rpm,
            live.max_rpm
        );
        return Ok(());
    }

    let host = Host::detect()?;
    let allowlist: Option<HashSet<String>> = match Calibration::load()? {
        Some(c) if c.matches_host(&host) => Some(c.sensors.into_keys().collect()),
        Some(_) => {
            eprintln!(
                "warning: calibration host mismatch; degraded trip — scanning all HID sensors"
            );
            None
        }
        None => {
            eprintln!("warning: no calibration; degraded trip — scanning all HID sensors");
            None
        }
    };

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::SeqCst))?;
    }

    let mut session = ManualFanSession::arm(fan, casing, md_info)?;
    emit_event(
        json,
        &SetEvent::Armed {
            fan,
            mode_key_type: &md_info.data_type,
        },
    );

    let mut emas: HashMap<String, AsymmetricEma> = HashMap::new();
    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let mut tripped: Option<(String, f64)> = None;
    let mut target_written_emitted = false;

    while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
        let temps = msf_hid::read_all().unwrap_or_default();

        let mut raw_max = f64::NEG_INFINITY;
        let mut ema_max = f64::NEG_INFINITY;
        for s in &temps {
            let in_scope = allowlist.as_ref().is_none_or(|a| a.contains(&s.name));
            if !in_scope {
                continue;
            }
            if s.celsius >= TRIP_THRESHOLD_C {
                tripped = Some((s.name.clone(), s.celsius));
            }
            if s.celsius > raw_max {
                raw_max = s.celsius;
            }
            let e = emas
                .entry(s.name.clone())
                .or_insert_with(|| AsymmetricEma::new(0.7, 0.15));
            let v = e.update(s.celsius);
            if v > ema_max {
                ema_max = v;
            }
        }

        if tripped.is_some() {
            break;
        }

        if !ema_max.is_finite() {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }

        let raw_target = curve.evaluate(ema_max);
        let target = clamp_rpm(raw_target, live.min_rpm, live.max_rpm);
        session.write_target(tg_info, target)?;

        if !target_written_emitted {
            emit_event(
                json,
                &SetEvent::TargetWritten {
                    fan,
                    target_rpm: target,
                },
            );
            target_written_emitted = true;
        }

        emit_event(
            json,
            &SetEvent::Tick {
                t: now_ms(),
                max_temp_c: ema_max,
                target_rpm: target,
            },
        );

        std::thread::sleep(Duration::from_secs(1));
    }

    if let Some((name, c)) = &tripped {
        emit_event(
            json,
            &SetEvent::Trip {
                sensor: name,
                celsius: *c,
            },
        );
    }

    emit_event(json, &SetEvent::RestoreStarted);
    let restore_ok = session.restore().unwrap_or(false);
    if restore_ok {
        emit_event(json, &SetEvent::RestoreDone);
    } else {
        emit_event(json, &SetEvent::RestoreFailed);
    }

    let (code, reason): (i32, &str) = if !restore_ok {
        (3, "restore_failed")
    } else if tripped.is_some() {
        (4, "sensor_trip")
    } else if stop.load(Ordering::SeqCst) {
        (0, "sigint")
    } else {
        (0, "ttl")
    };
    emit_event(json, &SetEvent::Exit { code, reason });
    std::process::exit(code);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_plist_round_trips_through_parser() {
        let cfg = PlistConfig {
            binary: PathBuf::from("/usr/local/bin/msf"),
            curve: PathBuf::from("/etc/msf/profile.toml"),
            fan: 0,
        };
        let body = render_plist(&cfg).unwrap();
        let value = plist::Value::from_reader(std::io::Cursor::new(&body)).unwrap();
        let dict = value.as_dictionary().unwrap();
        assert_eq!(dict.get("Label").unwrap().as_string(), Some(DAEMON_LABEL));
        assert_eq!(dict.get("RunAtLoad").unwrap().as_boolean(), Some(true));
        assert_eq!(dict.get("KeepAlive").unwrap().as_boolean(), Some(true));
        let args = dict.get("ProgramArguments").unwrap().as_array().unwrap();
        assert_eq!(args[0].as_string().unwrap(), "/usr/local/bin/msf");
        assert_eq!(args[1].as_string().unwrap(), "run");
        assert_eq!(args[2].as_string().unwrap(), "--curve");
        assert_eq!(args[3].as_string().unwrap(), "/etc/msf/profile.toml");
        assert_eq!(args[4].as_string().unwrap(), "--fan");
        assert_eq!(args[5].as_string().unwrap(), "0");
        assert_eq!(args[6].as_string().unwrap(), "--duration-secs");
        assert_eq!(
            args[7].as_string().unwrap(),
            DAEMON_DURATION_SECS.to_string()
        );
    }

    #[test]
    fn parse_curve_path_extracts_curve_arg() {
        let cfg = PlistConfig {
            binary: PathBuf::from("/usr/local/bin/msf"),
            curve: PathBuf::from("/etc/msf/profile.toml"),
            fan: 1,
        };
        let body = render_plist(&cfg).unwrap();
        let p = std::env::temp_dir().join(format!("msf-plist-{}.plist", std::process::id()));
        std::fs::write(&p, &body).unwrap();
        let recovered = parse_curve_path_from_plist(&p).unwrap();
        assert_eq!(recovered, PathBuf::from("/etc/msf/profile.toml"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn resolve_binary_uses_arg_when_given() {
        let p = PathBuf::from("/custom/msf");
        assert_eq!(resolve_binary(Some(p.clone())), p);
    }

    #[test]
    fn resolve_binary_with_uses_current_when_arg_is_none() {
        let v = resolve_binary_with(None, || Some(PathBuf::from("/cur/msf")));
        assert_eq!(v, PathBuf::from("/cur/msf"));
    }

    #[test]
    fn resolve_binary_with_falls_back_when_current_fails() {
        let v = resolve_binary_with(None, || None);
        assert_eq!(v, PathBuf::from("/usr/local/bin/msf"));
    }

    #[test]
    fn resolve_binary_with_prefers_arg_over_current() {
        let v = resolve_binary_with(Some(PathBuf::from("/arg/msf")), || {
            Some(PathBuf::from("/cur/msf"))
        });
        assert_eq!(v, PathBuf::from("/arg/msf"));
    }
}
