mod dop;
mod dsd;
mod native;
mod output;
mod player;
mod reader;
mod tui;

use crate::dsd::DsdRate;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::output::FormatLine;
use crate::player::{PlayOptions, Target};
use crate::reader::tags::TrackTags;

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
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u32).range(20..=10000))]
        buffer_ms: u32,
        /// Override the device IO buffer size, in frames
        #[arg(long)]
        buffer_frames: Option<u32>,
    },
    /// Browse and play DSD files in a terminal UI
    Tui {
        /// Directory to start browsing in; defaults to the working directory
        dir: Option<PathBuf>,
        /// Output device name fragment or UID; defaults to the system output device
        #[arg(short, long)]
        device: Option<String>,
        /// Leave the device shared with other apps instead of claiming it exclusively
        #[arg(long)]
        shared: bool,
        /// Size of the DoP queue between the reader and the audio callback
        #[arg(long, default_value_t = 500, value_parser = clap::value_parser!(u32).range(20..=10000))]
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
        Command::Tui {
            dir,
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
            tui::run(
                dir.unwrap_or_else(|| PathBuf::from(".")),
                device.as_deref(),
                options,
            )
        }
        Command::Devices { formats } => show_devices(formats),
        Command::Info { files } => show_info(&files),
    }
}

fn play_all(files: &[PathBuf], device: Option<&str>, options: &PlayOptions) -> Result<()> {
    let mut target = Target::resolve(device)?;
    let stop = Arc::new(AtomicBool::new(false));
    let interrupted = Arc::new(AtomicBool::new(false));
    let handler_stop = Arc::clone(&stop);
    let handler_interrupted = Arc::clone(&interrupted);
    // The handler runs on its own thread, so it can hand the device back itself. A second
    // signal means the caller is not willing to wait for the tail, so leave at once - but
    // still release the device, or it stays claimed until macOS notices the process died.
    // Exiting runs no destructors, so a natively held DAC has to be re-enumerated here too.
    ctrlc::set_handler(move || {
        if handler_interrupted.swap(true, Ordering::Relaxed) {
            eprintln!();
            output::stream::release_claimed_device();
            output::usb::device::release_claimed_dac();
            std::process::exit(130);
        }
        handler_stop.store(true, Ordering::Relaxed);
    })?;

    for file in files {
        let played = player::play(file, &mut target, options, &stop);
        // An interrupt can surface as an error from a half-opened device. The user asked to
        // stop, so that is not worth reporting as a failure.
        if interrupted.load(Ordering::Relaxed) {
            break;
        }
        played?;
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
        if !device.native_dsd.is_empty() {
            // Same shape as the dop line: the frame rate the clock runs at, then the DSD
            // rate it carries. Both the 44.1 and 48 kHz families reach a given multiplier,
            // so the rate is what tells them apart.
            let native: Vec<String> = device
                .native_dsd
                .iter()
                .filter_map(|hz| {
                    let multiplier = DsdRate::new(*hz).multiplier()?;
                    Some(format!("{}/DSD{multiplier}", hz / 32))
                })
                .collect();
            println!("    native    {}", native.join(" "));
        }
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

/// The tag lines a file has something to say on, in the order a listener reads them.
fn tag_rows(tags: &TrackTags) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    for (label, value) in [
        ("title", tags.title.clone()),
        ("artist", tags.artist.clone()),
        ("album", tags.album.clone()),
        ("track", tags.track.map(|number| number.to_string())),
    ] {
        if let Some(value) = value {
            rows.push((label, value));
        }
    }
    rows
}

fn show_info(files: &[PathBuf]) -> Result<()> {
    for file in files {
        let source = reader::open(file)?;
        let format = source.format();
        let seconds = source.duration_secs();
        println!("{}", file.display());
        for (label, value) in tag_rows(source.tags()) {
            println!("    {label:<11} {value}");
        }
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
