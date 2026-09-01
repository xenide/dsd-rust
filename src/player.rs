use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use rtrb::{Producer, RingBuffer};
use tracing::debug;

use crate::dop;
use crate::output::stream::{Output, Request};
use crate::output::{self, hal::Device};
use crate::reader::{self, DsdSource};

/// How long the DAC keeps receiving DoP silence after the music ends, so it does not pop.
const TAIL: Duration = Duration::from_millis(150);
const POLL: Duration = Duration::from_millis(100);

pub struct PlayOptions {
    pub device: Option<String>,
    pub exclusive: bool,
    pub buffer_ms: u32,
    pub buffer_frames: Option<u32>,
}

#[derive(Debug, Default)]
struct FeedState {
    frames_written: AtomicU64,
    finished: AtomicBool,
}

pub fn play(path: &Path, options: &PlayOptions, stop: &Arc<AtomicBool>) -> Result<()> {
    let source = reader::open(path)?;
    let format = source.format();
    let channels = format.channels as usize;
    let pcm_rate = format.rate.dop_pcm_rate();
    let total_frames = source.total_bytes_per_channel() / 2;

    let (device, device_name) = output::find_device(options.device.as_deref())?;
    let capacity = (pcm_rate as usize * channels * options.buffer_ms as usize / 1000).max(1 << 14);
    let (producer, consumer) = RingBuffer::<u16>::new(capacity);

    let request = Request {
        pcm_rate,
        channels: format.channels,
        exclusive: options.exclusive,
        buffer_frames: options.buffer_frames,
    };
    let mut output = Output::open(device, &request, consumer)
        .with_context(|| format!("{device_name} cannot play {}", format.rate))?;
    warn_about_volume(&device);

    println!(
        "{}  {}  {}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        format,
        source.container()
    );
    println!(
        "  -> {device_name}: DoP {pcm_rate} Hz, {} frame buffer, {}, {}{}",
        output.buffer_frames,
        transport_label(&output),
        if output.is_exclusive() {
            "exclusive"
        } else {
            "shared"
        },
        if output.mixing_disabled() {
            ", mixing off"
        } else {
            ""
        }
    );

    let feed = Arc::new(FeedState::default());
    let feeder = spawn_feeder(source, producer, Arc::clone(&feed), Arc::clone(stop));

    let prefill = (capacity / channels / 2) as u64;
    while feed.frames_written.load(Ordering::Relaxed) < prefill.min(total_frames)
        && !feed.finished.load(Ordering::Relaxed)
        && !stop.load(Ordering::Relaxed)
    {
        thread::sleep(Duration::from_millis(5));
    }

    output.start()?;
    let duration = source_duration(total_frames, pcm_rate);
    while !stop.load(Ordering::Relaxed) {
        let played = output.state.frames_played.load(Ordering::Relaxed);
        print_progress(played, total_frames, pcm_rate, duration);
        if feed.finished.load(Ordering::Relaxed)
            && played >= feed.frames_written.load(Ordering::Relaxed)
        {
            break;
        }
        thread::sleep(POLL);
    }

    output.state.silence.store(true, Ordering::Relaxed);
    thread::sleep(TAIL);
    output.stop();
    println!();

    let underruns = output.state.underrun_frames.load(Ordering::Relaxed);
    if underruns > 0 {
        debug!("{underruns} frames of DoP silence were inserted");
    }
    stop.store(true, Ordering::Relaxed);
    let result = feeder.join().unwrap_or_else(|_| Ok(()));
    stop.store(false, Ordering::Relaxed);
    result
}

fn transport_label(output: &Output) -> &'static str {
    if output.encoding.is_integer() {
        "integer"
    } else {
        "float32"
    }
}

fn source_duration(total_frames: u64, pcm_rate: u32) -> f64 {
    total_frames as f64 / f64::from(pcm_rate)
}

fn print_progress(played: u64, total_frames: u64, pcm_rate: u32, duration: f64) {
    let elapsed = source_duration(played.min(total_frames), pcm_rate);
    print!("\r  {} / {}   ", clock(elapsed), clock(duration));
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn warn_about_volume(device: &Device) {
    let Some(volume) = device.volume_scalar() else {
        return;
    };
    if volume < 1.0 {
        eprintln!(
            "warning: device volume is {:.0}%. If it is applied in software it will corrupt the \
             DoP stream; set it to maximum and use the DAC's own volume control.",
            volume * 100.0
        );
    }
}

fn spawn_feeder(
    mut source: Box<dyn DsdSource>,
    mut producer: Producer<u16>,
    feed: Arc<FeedState>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || {
        let channels = source.format().channels as usize;
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; source.chunk_bytes()].into(); channels];
        let mut payloads = Vec::new();
        let result = feed_loop(
            &mut source,
            &mut producer,
            &feed,
            &stop,
            &mut planes,
            &mut payloads,
        );
        feed.finished.store(true, Ordering::Relaxed);
        result
    })
}

fn feed_loop(
    source: &mut Box<dyn DsdSource>,
    producer: &mut Producer<u16>,
    feed: &FeedState,
    stop: &AtomicBool,
    planes: &mut [Box<[u8]>],
    payloads: &mut Vec<u16>,
) -> Result<()> {
    let channels = source.format().channels as usize;
    while !stop.load(Ordering::Relaxed) {
        let count = source.read(planes)?;
        if count == 0 {
            return Ok(());
        }
        payloads.clear();
        let slices: Vec<&[u8]> = planes.iter().map(|plane| &plane[..count]).collect();
        dop::pack_planes(&slices, payloads);

        let mut offset = 0;
        while offset < payloads.len() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let free = producer.slots();
            if free == 0 {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            let take = free.min(payloads.len() - offset);
            let chunk = producer.write_chunk_uninit(take)?;
            chunk.fill_from_iter(payloads[offset..offset + take].iter().copied());
            offset += take;
            feed.frames_written
                .fetch_add((take / channels) as u64, Ordering::Relaxed);
        }
    }
    Ok(())
}
