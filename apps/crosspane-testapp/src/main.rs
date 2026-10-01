//! A deterministic window and input-event fixture for platform tests.

mod events;
mod window;

use std::{fs::File, io::BufWriter, path::PathBuf, time::Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crosspane_testapp::pattern::{ImageSize, write_ppm};

#[derive(Debug, Parser)]
#[command(
    name = "crosspane-testapp",
    about = "Pixel pattern and input-event fixture"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Write the CPU reference pattern as binary PPM.
    Pattern {
        #[arg(long)]
        size: ImageSize,
        #[arg(long)]
        out: PathBuf,
    },
    /// Show the device-pixel pattern and record window input as JSON lines.
    Window {
        #[arg(long, default_value = "crosspane-testapp")]
        title: String,
        #[arg(long, default_value = "800x600")]
        size: ImageSize,
        #[arg(long)]
        events: Option<PathBuf>,
    },
}

fn main() -> Result<()> {
    let start = Instant::now();
    match Cli::parse().command {
        Command::Pattern { size, out } => {
            let file = File::create(&out).context("creating CPU reference PPM")?;
            write_ppm(BufWriter::new(file), size).context("writing CPU reference PPM")
        }
        Command::Window {
            title,
            size,
            events,
        } => window::run(title, size, events.as_deref(), start),
    }
}
