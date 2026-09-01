mod dop;
mod dsd;
mod output;
mod player;
mod reader;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::output::FormatLine;
use crate::player::{PlayOptions, Target};

/// Bit-perfect DSD player. DSD is carried to the DAC untouched, as DoP 1.1.
#[derive(Debug, Parser)]
#[command(name = "dsd-rust", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Play DSF or DSDIFF files in order
    Play {
        #[arg(required = true)]
        files: Vec<PathBuf>,
        /// Output device name fragment or UID; defaults to the system output device
        #[arg(short, long)]
        device: Option<String>,
        /// Leave the device shared with other apps instead of claiming it exclusively
        #[arg(long)]
        shared: bool,
        /// Size of the DoP queue between the reader and the audio callback
        #[arg(long, default_value_t = 250, value_parser = clap::value_parser!(u32).range(20..=5000))]
        buffer_ms: u32,
        /// Override the device IO buffer size, in frames
        #[arg(long)]
        buffer_frames: Option<u32>,
    },
    /// List output devices and the DoP rates they accept
    Devices {
        /// Also list every stream format each device advertises
        #[arg(long)]
        formats: bool,
    },
    /// Print what a DSD file contains
    Info {
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_target(false)
        .init();

    match Cli::parse().command {
        Command::Play {
            files,
            device,
            shared,
            buffer_ms,
            buffer_frames,
        } => {
            let options = PlayOptions {
                exclusive: !shared,
                buffer_ms,
                buffer_frames,
            };
            play_all(&files, device.as_deref(), &options)
        }
        Command::Devices { formats } => show_devices(formats),
        Command::Info { files } => show_info(&files),
    }
}

fn play_all(files: &[PathBuf], device: Option<&str>, options: &PlayOptions) -> Result<()> {
    let target = Target::resolve(device)?;
    let stop = Arc::new(AtomicBool::new(false));
    let interrupted = Arc::new(AtomicBool::new(false));
    let handler_stop = Arc::clone(&stop);
    let handler_interrupted = Arc::clone(&interrupted);
    ctrlc::set_handler(move || {
        handler_interrupted.store(true, Ordering::Relaxed);
        handler_stop.store(true, Ordering::Relaxed);
    })?;

    for file in files {
        player::play(file, &target, options, &stop)?;
        if interrupted.load(Ordering::Relaxed) {
            break;
        }
    }
    Ok(())
}

fn show_devices(formats: bool) -> Result<()> {
    for device in output::list_devices()? {
        let marker = if device.is_default { "*" } else { " " };
        let rates = if device.dop_rates.is_empty() {
            "none".to_owned()
        } else {
            device
                .dop_rates
                .iter()
                .map(|rate| format!("{}/DSD{}", rate, rate * 16 / 44_100))
                .collect::<Vec<_>>()
                .join(" ")
        };
        println!("{marker} {}", device.name);
        println!("    uid       {}", device.uid);
        println!("    current   {:.0} Hz", device.current_rate);
        println!("    dop       {rates}");
        if let Some(owner) = device.hog_owner {
            println!("    exclusive held by process {owner}");
        }
        if formats {
            show_formats(&device.name)?;
        }
    }
    Ok(())
}

fn show_formats(name: &str) -> Result<()> {
    let (device, _) = output::find_device(Some(name))?;
    for (index, stream) in output::stream_formats(&device)?.iter().enumerate() {
        println!("    stream {index}");
        println!(
            "      in use physical  {}",
            FormatLine(stream.current_physical)
        );
        println!(
            "      in use virtual   {}",
            FormatLine(stream.current_virtual)
        );
        for (label, list) in [
            ("physical", &stream.physical),
            ("virtual", &stream.virtual_formats),
        ] {
            println!("      available {label}");
            for ranged in list {
                println!("        {}", FormatLine(ranged.mFormat));
            }
        }
    }
    Ok(())
}

fn show_info(files: &[PathBuf]) -> Result<()> {
    for file in files {
        let source = reader::open(file)?;
        let format = source.format();
        let seconds = source.duration_secs();
        println!("{}", file.display());
        println!("    container   {}", source.container());
        println!("    format      {format}");
        println!(
            "    duration    {}:{:05.2}",
            (seconds / 60.0) as u64,
            seconds % 60.0
        );
        println!("    dop rate    {} Hz, 24 bit", format.rate.dop_pcm_rate());
        println!(
            "    audio       {} bytes per channel",
            source.total_bytes_per_channel()
        );
    }
    Ok(())
}
