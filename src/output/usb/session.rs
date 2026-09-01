//! Playing one file over the native DSD path.
//!
//! This is the DoP session's sibling: a reader thread packs planar DSD into native subslots
//! and a ring feeds the isochronous engine. It exists separately because the two carry
//! different units -- DoP queues 24-bit words, native queues raw bytes.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rtrb::{Producer, RingBuffer};

use crate::dsd::DsdFormat;
use crate::native;
use crate::output::stream::PlaybackState;
use crate::output::usb::device::Dac;
use crate::output::usb::stream::NativeStream;
use crate::player::{DeviceInfo, Progress, TrackInfo};
use crate::reader::{self, DsdSource};

/// How long the DAC keeps receiving DSD silence after the music ends, so it does not pop.
const TAIL: Duration = Duration::from_millis(150);
/// Smallest ring worth having, whatever `--buffer-ms` says.
const MIN_RING: usize = 1 << 16;

#[derive(Debug, Default)]
struct FeedState {
    frames_written: AtomicU64,
    finished: AtomicBool,
}

/// What a native session is playing and where.
pub struct NativeInfo {
    pub name: String,
    pub format: DsdFormat,
    pub container: &'static str,
    pub duration: f64,
    pub frame_rate: u32,
    pub total_frames: u64,
}

pub struct NativeSession {
    stream: NativeStream,
    feeder: Option<JoinHandle<Result<()>>>,
    feed: Arc<FeedState>,
    stop: Arc<AtomicBool>,
    state: Arc<PlaybackState>,
    queue_frames: u64,
    pub info: NativeInfo,
}

/// Pick the DAC to play to: the one whose name matches, or the only one present.
pub fn find_dac(query: Option<&str>) -> Result<Dac> {
    let mut dacs = Dac::discover()?;
    if dacs.is_empty() {
        bail!("no attached USB DAC advertises a native DSD alternate setting");
    }
    let Some(query) = query else {
        if dacs.len() > 1 {
            let names: Vec<&str> = dacs.iter().map(|dac| dac.name.as_str()).collect();
            bail!(
                "several DACs can play native DSD ({}); name one with --device",
                names.join(", ")
            );
        }
        return Ok(dacs.remove(0));
    };

    let needle = query.to_lowercase();
    let found = dacs
        .iter()
        .position(|dac| dac.name.to_lowercase().contains(&needle));
    match found {
        Some(index) => Ok(dacs.remove(index)),
        None => {
            let names: Vec<&str> = dacs.iter().map(|dac| dac.name.as_str()).collect();
            bail!(
                "no native DSD device matches {query:?}; available: {}",
                names.join(", ")
            )
        }
    }
}

impl NativeSession {
    /// Open the file, take the DAC away from `usbaudiod`, prefill, and start streaming.
    pub fn open(
        path: &Path,
        query: Option<&str>,
        buffer_ms: u32,
        stop: &Arc<AtomicBool>,
    ) -> Result<Self> {
        let source = reader::open(path)?;
        let format = source.format();
        let channels = format.channels as usize;
        let dsd_rate = format.rate.hz();

        let dac = find_dac(query)?;
        let max = dac.native.max_dsd_rate(format.channels.into());
        if dsd_rate > max {
            bail!(
                "{}: native endpoint carries at most {:.4} MHz per channel, but this file is {}",
                dac.name,
                f64::from(max) / 1_000_000.0,
                format.rate
            );
        }
        let name = dac.name.clone();

        let frame_bytes = native::frame_bytes(channels);
        let capacity =
            (format.rate.native_frame_rate() as usize * frame_bytes * buffer_ms as usize / 1000)
                .max(MIN_RING);
        let (producer, consumer) = RingBuffer::<u8>::new(capacity);

        let info = NativeInfo {
            name: name.clone(),
            format,
            container: source.container(),
            duration: source.duration_secs(),
            frame_rate: format.rate.native_frame_rate(),
            total_frames: source.total_bytes_per_channel() / native::BYTES_PER_SUBSLOT as u64,
        };

        let feed = Arc::new(FeedState::default());
        let feeder = spawn_feeder(source, producer, Arc::clone(&feed), Arc::clone(stop));

        // Prefill before taking the device, so the first transfers carry music.
        let target = (capacity / frame_bytes / 2) as u64;
        while feed.frames_written.load(Ordering::Relaxed) < target.min(info.total_frames)
            && !feed.finished.load(Ordering::Relaxed)
            && !stop.load(Ordering::Relaxed)
        {
            thread::sleep(Duration::from_millis(5));
        }

        let held = dac
            .acquire()
            .with_context(|| format!("{name} cannot be claimed for native DSD"))?;
        let state = Arc::new(PlaybackState::default());
        let stream = NativeStream::start(held, consumer, channels, dsd_rate, Arc::clone(&state))
            .with_context(|| format!("{name} cannot play {} natively", format.rate))?;

        Ok(Self {
            stream,
            feeder: Some(feeder),
            feed,
            stop: Arc::clone(stop),
            state,
            queue_frames: (capacity / frame_bytes) as u64,
            info,
        })
    }

    /// Hold the queue where it is. The engine keeps sending DSD silence, so the DAC stays
    /// locked and resuming costs no relock.
    pub fn set_paused(&self, paused: bool) {
        self.state.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.state.paused.load(Ordering::Relaxed)
    }

    pub fn track(&self) -> TrackInfo {
        TrackInfo {
            container: self.info.container,
            format: self.info.format,
            duration: self.info.duration,
            total_frames: self.info.total_frames,
            bytes_per_channel: self.info.total_frames * native::BYTES_PER_SUBSLOT as u64,
        }
    }

    pub fn device(&self) -> DeviceInfo {
        DeviceInfo {
            name: self.info.name.clone(),
            carrier: "native DSD",
            bits: 32,
            pcm_rate: self.info.frame_rate,
            // Audio frames in flight per isochronous transfer.
            buffer_frames: self.info.frame_rate / 8_000 * 32,
            transport: "integer",
            exclusive: true,
            mixing_disabled: true,
            volume: None,
        }
    }

    pub fn progress(&self) -> Progress {
        let frames_played = self.state.frames_played.load(Ordering::Relaxed);
        let written = self.feed.frames_written.load(Ordering::Relaxed);
        Progress {
            elapsed: self.elapsed(),
            frames_played,
            underrun_frames: self.underrun_frames(),
            queued_frames: written.saturating_sub(frames_played),
            queue_frames: self.queue_frames,
        }
    }

    pub fn elapsed(&self) -> f64 {
        let played = self.state.frames_played.load(Ordering::Relaxed);
        played.min(self.info.total_frames) as f64 / f64::from(self.info.frame_rate)
    }

    pub fn underrun_frames(&self) -> u64 {
        self.state.underrun_frames.load(Ordering::Relaxed)
    }

    /// True once the whole file is queued and the engine has consumed all of it.
    pub fn is_complete(&self) -> bool {
        self.feed.finished.load(Ordering::Relaxed)
            && self.state.frames_played.load(Ordering::Relaxed)
                >= self.feed.frames_written.load(Ordering::Relaxed)
    }

    pub fn fully_queued(&self) -> bool {
        self.feed.finished.load(Ordering::Relaxed)
    }

    /// Play out the closing silence, stop the engine, and hand the DAC back.
    pub fn finish(mut self) -> Result<()> {
        self.state.silence.store(true, Ordering::Relaxed);
        thread::sleep(TAIL);
        self.stream.stop();
        self.stop.store(true, Ordering::Relaxed);
        let result = match self.feeder.take() {
            Some(feeder) => feeder.join().unwrap_or_else(|_| Ok(())),
            None => Ok(()),
        };
        self.stop.store(false, Ordering::Relaxed);
        result
    }
}

impl Drop for NativeSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(feeder) = self.feeder.take() {
            let _ = feeder.join();
        }
        self.stop.store(false, Ordering::Relaxed);
    }
}

fn spawn_feeder(
    mut source: Box<dyn DsdSource>,
    mut producer: Producer<u8>,
    feed: Arc<FeedState>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<Result<()>> {
    thread::spawn(move || {
        let channels = source.format().channels as usize;
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; source.chunk_bytes()].into(); channels];
        let mut packed = Vec::new();
        let result = feed_loop(
            &mut source,
            &mut producer,
            &feed,
            &stop,
            &mut planes,
            &mut packed,
        );
        feed.finished.store(true, Ordering::Relaxed);
        result
    })
}

fn feed_loop(
    source: &mut Box<dyn DsdSource>,
    producer: &mut Producer<u8>,
    feed: &FeedState,
    stop: &AtomicBool,
    planes: &mut [Box<[u8]>],
    packed: &mut Vec<u8>,
) -> Result<()> {
    let channels = source.format().channels as usize;
    let frame_bytes = native::frame_bytes(channels);
    while !stop.load(Ordering::Relaxed) {
        let count = source.read(planes)?;
        if count == 0 {
            return Ok(());
        }
        packed.clear();
        let slices: Vec<&[u8]> = planes.iter().map(|plane| &plane[..count]).collect();
        native::pack_planes(&slices, packed);

        let mut offset = 0;
        while offset < packed.len() {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let free = producer.slots();
            if free == 0 {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            let take = free.min(packed.len() - offset);
            let chunk = producer.write_chunk_uninit(take)?;
            chunk.fill_from_iter(packed[offset..offset + take].iter().copied());
            offset += take;
            feed.frames_written
                .fetch_add((take / frame_bytes) as u64, Ordering::Relaxed);
        }
    }
    Ok(())
}
