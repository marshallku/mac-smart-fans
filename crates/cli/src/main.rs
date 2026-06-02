use anyhow::Result;
use clap::{Parser, Subcommand};
use msf_core::{Reading, SensorSource};
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
    /// Print sensor + fan readings (read-only, no sudo needed)
    Monitor {
        #[arg(long)]
        json: bool,

        #[arg(long, default_value_t = 1.0)]
        interval_secs: f64,

        #[arg(long, default_value_t = 0)]
        ticks: u32,
    },
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
        } => run_monitor(json, interval_secs, ticks),
    }
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

fn run_monitor(json: bool, interval_secs: f64, ticks: u32) -> Result<()> {
    let interval = Duration::from_secs_f64(interval_secs.max(0.05));
    let mut count = 0u32;
    loop {
        let readings = collect_readings();
        if json {
            for r in &readings {
                println!("{}", serde_json::to_string(r)?);
            }
        } else {
            print_table(&readings);
        }
        count += 1;
        if ticks > 0 && count >= ticks {
            return Ok(());
        }
        std::thread::sleep(interval);
    }
}

fn print_table(readings: &[Reading]) {
    println!("{:<44} {:>4} {:>10} {:>5}", "name", "src", "value", "unit");
    for r in readings {
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
    println!();
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
