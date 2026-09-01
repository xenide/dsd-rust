use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crate::dop;
use crate::dsd::DsdFormat;
use crate::output::stream::{DeviceBusy, Output, Request};
use crate::output::{self, hal::Device};
use crate::reader::{self, DsdSource};
use anyhow::{Context, Result};
use rtrb::{Producer, RingBuffer};
use tracing::debug;

/// How long the DAC keeps receiving DoP silence after the music ends, so it does not pop.
const TAIL: Duration = Duration::from_millis(150);
const POLL: Duration = Duration::from_millis(20);
/// A track gets this many goes at claiming a device that is still switching rate.
const SETUP_ATTEMPTS: u32 = 4;
const SETTLE: Duration = Duration::from_millis(300);

pub struct PlayOptions {
    pub exclusive: bool,
    pub buffer_ms: u32,
    pub buffer_frames: Option<u32>,
}

/// The device a playlist plays to, resolved once: holding a device exclusively moves the
/// system default elsewhere, so re-resolving between tracks would pick the wrong one.
pub struct Target {
    pub device: Device,
    pub name: String,
}

impl Target {
    pub fn resolve(query: Option<&str>) -> Result<Self> {
        let (device, name) = output::find_device(query)?;
        Ok(Self { device, name })
    }
}

#[derive(Debug, Default)]
struct FeedState {
    frames_written: AtomicU64,
    finished: AtomicBool,
}

/// What the file being played contains.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub container: &'static str,
    pub format: DsdFormat,
    pub duration: f64,
    pub total_frames: u64,
    pub bytes_per_channel: u64,
}

/// What the device settled on for this track.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub pcm_rate: u32,
    pub buffer_frames: u32,
    pub transport: &'static str,
    pub exclusive: bool,
    pub mixing_disabled: bool,
    pub volume: Option<f32>,
}

/// A snapshot of a running session, cheap enough to take on every UI frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct Progress {
    pub elapsed: f64,
    pub frames_played: u64,
    pub underrun_frames: u64,
    pub queued_frames: u64,
    pub queue_frames: u64,
}

impl Progress {
    pub fn queue_fill(&self) -> f64 {
        if self.queue_frames == 0 {
            return 0.0;
        }
        self.queued_frames as f64 / self.queue_frames as f64
    }
}

/// One track playing to one device: the reader thread, the DoP queue, and the render callback.
pub struct Session {
    output: Output,
    feeder: Option<thread::JoinHandle<Result<()>>>,
    feed: Arc<FeedState>,
    stop: Arc<AtomicBool>,
    queue_frames: u64,
    pub track: TrackInfo,
    pub device: DeviceInfo,
}

impl Session {
    /// Open the file, take over the device, prefill the queue, and start the callback.
    pub fn open(
        path: &Path,
        target: &Target,
        options: &PlayOptions,
        stop: &Arc<AtomicBool>,
    ) -> Result<Self> {
        let source = reader::open(path)?;
        let format = source.format();
        let channels = format.channels as usize;
        let pcm_rate = format.rate.dop_pcm_rate();
        let track = TrackInfo {
            container: source.container(),
            format,
            duration: source.duration_secs(),
            total_frames: source.total_bytes_per_channel() / 2,
            bytes_per_channel: source.total_bytes_per_channel(),
        };

        let device = target.device;
        let capacity =
            (pcm_rate as usize * channels * options.buffer_ms as usize / 1000).max(1 << 14);
        let (producer, consumer) = RingBuffer::<u16>::new(capacity);

        let request = Request {
            pcm_rate,
            channels: format.channels,
            exclusive: options.exclusive,
            buffer_frames: options.buffer_frames,
        };
        let mut output = Output::open(device, &request, consumer)
            .with_context(|| format!("{} cannot play {}", target.name, format.rate))?;

        let info = DeviceInfo {
            name: target.name.clone(),
            pcm_rate,
            buffer_frames: output.buffer_frames,
            transport: if output.encoding.is_integer() {
                "integer"
            } else {
                "float32"
            },
            exclusive: output.is_exclusive(),
            mixing_disabled: output.mixing_disabled(),
            volume: (!output.encoding.is_integer())
                .then(|| device.volume_scalar())
                .flatten(),
        };

        let feed = Arc::new(FeedState::default());
        let feeder = spawn_feeder(source, producer, Arc::clone(&feed), Arc::clone(stop));

        let prefill = (capacity / channels / 2) as u64;
        while feed.frames_written.load(Ordering::Relaxed) < prefill.min(track.total_frames)
            && !feed.finished.load(Ordering::Relaxed)
            && !stop.load(Ordering::Relaxed)
        {
            thread::sleep(Duration::from_millis(5));
        }

        output.start()?;
        Ok(Self {
            output,
            feeder: Some(feeder),
            feed,
            stop: Arc::clone(stop),
            queue_frames: (capacity / channels) as u64,
            track,
            device: info,
        })
    }

    /// Open a track, giving a device that is still settling from the previous one more goes.
    pub fn open_retrying(
        path: &Path,
        target: &Target,
        options: &PlayOptions,
        stop: &Arc<AtomicBool>,
    ) -> Result<Self> {
        let mut last = None;
        for attempt in 1..=SETUP_ATTEMPTS {
            let error = match Self::open(path, target, options, stop) {
                Ok(session) => return Ok(session),
                Err(error) => error,
            };
            if attempt == SETUP_ATTEMPTS || error.downcast_ref::<DeviceBusy>().is_none() {
                return Err(error);
            }
            debug!("{error}; retrying {}", path.display());
            thread::sleep(SETTLE);
            last = Some(error);
        }
        Err(last.expect("the loop runs at least once"))
    }

    pub fn progress(&self) -> Progress {
        let frames_played = self.output.state.frames_played.load(Ordering::Relaxed);
        let written = self.feed.frames_written.load(Ordering::Relaxed);
        Progress {
            elapsed: frames_played.min(self.track.total_frames) as f64
                / f64::from(self.device.pcm_rate),
            frames_played,
            underrun_frames: self.output.state.underrun_frames.load(Ordering::Relaxed),
            queued_frames: written.saturating_sub(frames_played),
            queue_frames: self.queue_frames,
        }
    }

    /// True once the whole file has been queued and the callback has consumed all of it.
    pub fn is_complete(&self) -> bool {
        self.feed.finished.load(Ordering::Relaxed)
            && self.output.state.frames_played.load(Ordering::Relaxed)
                >= self.feed.frames_written.load(Ordering::Relaxed)
    }

    /// Hold the queue where it is. The callback keeps sending DoP silence, so the DAC stays
    /// locked to DSD and resuming does not cost it a relock.
    pub fn set_paused(&self, paused: bool) {
        self.output.state.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.output.state.paused.load(Ordering::Relaxed)
    }

    /// Play out the closing silence, stop the callback, and join the reader thread.
    pub fn finish(mut self) -> Result<()> {
        self.output.state.silence.store(true, Ordering::Relaxed);
        thread::sleep(TAIL);
        self.output.stop();
        self.stop.store(true, Ordering::Relaxed);
        let result = match self.feeder.take() {
            Some(feeder) => feeder.join().unwrap_or_else(|_| Ok(())),
            None => Ok(()),
        };
        self.stop.store(false, Ordering::Relaxed);
        result
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(feeder) = self.feeder.take() {
            let _ = feeder.join();
        }
        self.stop.store(false, Ordering::Relaxed);
    }
}

/// Play one file, printing progress until it ends or `stop` is set.
pub fn play(
    path: &Path,
    target: &Target,
    options: &PlayOptions,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let session = Session::open_retrying(path, target, options, stop)?;
    warn_about_volume(&session.device);

    println!(
        "{}  {}  {}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        session.track.format,
        session.track.container
    );
    println!(
        "  -> {}: DoP {} Hz, {} frame buffer, {}, {}{}",
        session.device.name,
        session.device.pcm_rate,
        session.device.buffer_frames,
        session.device.transport,
        if session.device.exclusive {
            "exclusive"
        } else {
            "shared"
        },
        if session.device.mixing_disabled {
            ", mixing off"
        } else {
            ""
        }
    );

    let duration = session.track.duration;
    let mut dropouts = 0;
    let mut shown = u64::MAX;
    while !stop.load(Ordering::Relaxed) {
        let progress = session.progress();
        if session.is_complete() {
            break;
        }
        if !session.feed.finished.load(Ordering::Relaxed) {
            // Silence sent once the file is fully queued is the tail, not a dropout.
            dropouts = progress.underrun_frames;
        }
        if progress.elapsed as u64 != shown {
            shown = progress.elapsed as u64;
            print_progress(progress.elapsed, duration);
        }
        thread::sleep(POLL);
    }

    let pcm_rate = session.device.pcm_rate;
    session.finish()?;
    print_progress(duration, duration);
    println!();

    if dropouts > 0 {
        eprintln!(
            "  {dropouts} frames of DoP silence filled underruns ({:.0} ms); raise --buffer-ms",
            dropouts as f64 * 1000.0 / f64::from(pcm_rate)
        );
    }
    Ok(())
}

fn print_progress(elapsed: f64, duration: f64) {
    print!("\r  {} / {}   ", clock(elapsed), clock(duration));
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

pub fn clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// A software volume scales the samples and so destroys the DoP markers. Integer transport
/// has no gain stage at all, and a DAC that applies its own volume never sees one either, so
/// the warning is only worth making when the samples pass through Core Audio as float.
fn warn_about_volume(device: &DeviceInfo) {
    let Some(volume) = device.volume else {
        return;
    };
    if volume < 1.0 {
        eprintln!(
            "warning: device volume is {:.0}%. If macOS applies it in software the DAC will lose \
             DSD lock and play noise; if the DAC applies it itself, playback is unaffected.",
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
