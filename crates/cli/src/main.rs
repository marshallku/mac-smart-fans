use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "msf", version, about = "Mac Smart Fans — Apple Silicon thermal monitor and fan control")]
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
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Monitor { json, interval_secs } => run_monitor(json, interval_secs),
    }
}

fn run_monitor(_json: bool, _interval_secs: f64) -> Result<()> {
    let hid = msf_hid::read_all()?;
    let fans = msf_smc::read_fans()?;
    println!(
        "[skeleton] hid_sensors={} fan_count={} — FFI implementation lands next cycle",
        hid.len(),
        fans.len()
    );
    Ok(())
}
