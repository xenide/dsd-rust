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
use crate::output::usb::descriptors::NativeDsd;
use crate::output::usb::device::{Dac, Held};
use crate::output::usb::stream::NativeStream;
use crate::player::{DeviceInfo, Progress, Target, TrackInfo};
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
    /// Straight from the reader. `total_frames` divides it, so storing the frame count
    /// instead would round a file that is not a whole number of frames down.
    pub bytes_per_channel: u64,
}

impl NativeInfo {
    const fn total_frames(&self) -> u64 {
        self.bytes_per_channel / native::BYTES_PER_SUBSLOT as u64
    }
}

/// The reader thread filling the ring, stopped and joined when this is dropped.
///
/// It owns its own stop flag rather than sharing the caller's: the caller's carries a
/// Ctrl-C, and clearing that on the way out would swallow the interrupt.
struct Feeder {
    handle: Option<JoinHandle<Result<()>>>,
    stop: Arc<AtomicBool>,
}

impl Feeder {
    fn spawn(source: Box<dyn DsdSource>, producer: Producer<u8>, feed: Arc<FeedState>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_feeder(source, producer, feed, Arc::clone(&stop));
        Self {
            handle: Some(handle),
            stop,
        }
    }

    /// Stop reading and wait, returning what the reader ended with.
    fn join(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        match self.handle.take() {
            Some(handle) => handle.join().unwrap_or_else(|_| Ok(())),
            None => Ok(()),
        }
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

pub struct NativeSession {
    stream: NativeStream,
    feeder: Feeder,
    feed: Arc<FeedState>,
    state: Arc<PlaybackState>,
    queue_frames: u64,
    pub info: NativeInfo,
}

/// Pick the DAC the resolved target names.
///
/// `device_name` is the Core Audio name the target resolved to, and `query` is what the user
/// typed, which may be a fragment or a UID. Either can name the DAC, so both are tried --
/// but nothing else is: falling back to "the only DAC attached" would stream to a device the
/// user did not choose.
pub fn find_dac(query: Option<&str>, device_name: &str) -> Result<Dac> {
    let mut dacs = Dac::discover()?;
    if dacs.is_empty() {
        bail!("no attached USB DAC advertises a native DSD alternate setting");
    }

    let mut found = None;
    for name in query.into_iter().chain([device_name]) {
        found = dacs.iter().position(|dac| dac.matches(name));
        if found.is_some() {
            break;
        }
    }
    let Some(index) = found else {
        let names: Vec<&str> = dacs.iter().map(|dac| dac.name.as_str()).collect();
        bail!(
            "{device_name} is not a USB DAC that plays native DSD; these are, and \
             --device names one: {}",
            names.join(", ")
        );
    };
    Ok(dacs.remove(index))
}

/// The DAC a track will play to: one the target is already holding, or one still to be
/// claimed. Only the second costs a race with `usbaudiod`.
enum Claim {
    Held(Held),
    Free(Dac),
}

impl Claim {
    fn name(&self) -> &str {
        match self {
            Self::Held(held) => &held.name,
            Self::Free(dac) => &dac.name,
        }
    }

    fn native(&self) -> NativeDsd {
        match self {
            Self::Held(held) => held.native,
            Self::Free(dac) => dac.native,
        }
    }

    /// The interfaces to stream on, taking them from `usbaudiod` if they are not held yet.
    fn hold(self) -> Result<Held> {
        match self {
            Self::Held(held) => Ok(held),
            Self::Free(dac) => {
                let name = dac.name.clone();
                dac.acquire()
                    .with_context(|| format!("{name} cannot be claimed for native DSD"))
            }
        }
    }
}

impl NativeSession {
    /// Open the file, take the DAC away from `usbaudiod`, prefill, and start streaming.
    ///
    /// A DAC the target is already holding is reused as it stands. Handing one back
    /// re-enumerates it, and the claim after that would have to race `usbaudiod` for a
    /// window that the re-enumeration has already closed, so the race is run once per
    /// playlist rather than once per track.
    pub fn open(
        path: &Path,
        target: &mut Target,
        buffer_ms: u32,
        stop: &Arc<AtomicBool>,
    ) -> Result<Self> {
        let source = reader::open(path)?;
        let format = source.format();
        let channels = format.channels as usize;
        let dsd_rate = format.rate.hz();

        let claim = match target.dac.take() {
            Some(held) => Claim::Held(held),
            None => Claim::Free(find_dac(target.query.as_deref(), &target.name)?),
        };
        // A file the DAC cannot carry puts the claim straight back, so the next track still
        // finds it held rather than paying for a fresh race.
        if let Err(error) = claim.native().accepts(format) {
            let name = claim.name().to_owned();
            if let Claim::Held(held) = claim {
                target.dac = Some(held);
            }
            return Err(error.context(name));
        }
        let name = claim.name().to_owned();

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
            bytes_per_channel: source.total_bytes_per_channel(),
        };

        let feed = Arc::new(FeedState::default());
        // Anything below that returns early drops this, which stops and joins the reader.
        let feeder = Feeder::spawn(source, producer, Arc::clone(&feed));

        // Prefill before taking the device, so the first transfers carry music.
        let prefill = (capacity / frame_bytes / 2) as u64;
        while feed.frames_written.load(Ordering::Relaxed) < prefill.min(info.total_frames())
            && !feed.finished.load(Ordering::Relaxed)
            && !stop.load(Ordering::Relaxed)
        {
            thread::sleep(Duration::from_millis(5));
        }
        // Claiming the DAC re-enumerates it and then races usbaudiod for its interfaces,
        // which is not worth starting for a track that has already been cancelled. A claim
        // already in hand is dropped here, which is what a cancelled playlist wants anyway.
        if stop.load(Ordering::Relaxed) {
            bail!("{name}: interrupted before the DAC was claimed");
        }

        let held = claim.hold()?;
        let state = Arc::new(PlaybackState::default());
        let stream = NativeStream::start(held, consumer, channels, dsd_rate, Arc::clone(&state))
            .with_context(|| format!("{name} cannot play {} natively", format.rate))?;

        Ok(Self {
            stream,
            feeder,
            feed,
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
            total_frames: self.info.total_frames(),
            bytes_per_channel: self.info.bytes_per_channel,
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
        played.min(self.info.total_frames()) as f64 / f64::from(self.info.frame_rate)
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

    /// True when the engine gave up before the file was played out.
    ///
    /// A DAC that rejects a transfer ends the chain on the engine thread, which the session
    /// otherwise has no way of noticing: it would keep reporting a track that is playing
    /// nothing, and hold the DAC for as long as it did so.
    pub fn has_stalled(&self) -> bool {
        !self.stream.is_running() && !self.is_complete()
    }

    pub fn fully_queued(&self) -> bool {
        self.feed.finished.load(Ordering::Relaxed)
    }

    /// Play out the closing silence and stop the engine, handing the still-claimed DAC back
    /// to the caller. It comes back even when the reader ended in an error: a claim dropped
    /// on an error path costs the next track the same race as any other.
    pub fn finish(mut self) -> (Option<Held>, Result<()>) {
        self.state.silence.store(true, Ordering::Relaxed);
        thread::sleep(TAIL);
        let held = self.stream.stop();
        (held, self.feeder.join())
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

#[cfg(test)]
mod tests {
    use crate::dsd::{DsdFormat, DsdRate};
    use crate::output::usb::session::NativeInfo;

    fn info(bytes_per_channel: u64) -> NativeInfo {
        NativeInfo {
            name: "Cayin RU7".to_owned(),
            format: DsdFormat {
                rate: DsdRate::new(11_289_600),
                channels: 2,
            },
            container: "DSF",
            duration: 1.0,
            frame_rate: 352_800,
            bytes_per_channel,
        }
    }

    #[test]
    fn a_file_that_is_not_a_whole_number_of_frames_keeps_every_byte_it_has() {
        // One byte short of 101 frames: the frame count floors, the byte count must not.
        let info = info(403);

        assert_eq!(info.total_frames(), 100);
        assert_eq!(info.bytes_per_channel, 403);
    }

    #[test]
    fn a_whole_number_of_frames_reports_four_bytes_each() {
        let info = info(404);

        assert_eq!(info.total_frames(), 101);
        assert_eq!(info.bytes_per_channel, 404);
    }
}
