use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::dop;
use crate::dsd::DsdFormat;
use crate::dsd::DsdRate;
use crate::output::encoding::Carrier;
use crate::output::stream::{
    DeviceBusy, Output, Request, supported_dop_rates, supported_native_rates,
};
use crate::output::usb::device::Held;
use crate::output::usb::session::NativeSession;
use crate::output::{self, hal::Device};
use crate::reader::tags::TrackTags;
use crate::reader::{self, DsdSource};
use anyhow::{Context, Result, bail};
use rtrb::{Producer, RingBuffer};
use tracing::{debug, warn};

/// How long the DAC keeps receiving DoP silence after the music ends, so it does not pop.
const TAIL: Duration = Duration::from_millis(150);
const POLL: Duration = Duration::from_millis(20);
/// A track gets this many goes at claiming a device that is still switching rate.
const SETUP_ATTEMPTS: u32 = 4;
const SETTLE: Duration = Duration::from_millis(300);
/// How long to wait for a DAC to reappear in Core Audio after a native claim is released.
const REATTACH_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a seek gives the reader to get ahead again before the queue is served.
pub(crate) const PREFILL_TIMEOUT: Duration = Duration::from_millis(300);
/// How long a parked reader sleeps between looking at why it was parked.
pub(crate) const PARK: Duration = Duration::from_millis(5);
/// DSD bytes per channel a DoP frame carries.
const DOP_BYTES_PER_FRAME: u64 = 2;

/// Where `delta` seconds from `elapsed` lands, as DSD bytes per channel, clamped to the file.
pub(crate) fn seek_position(elapsed: f64, delta: f64, rate: DsdRate, total_bytes: u64) -> u64 {
    let seconds = (elapsed + delta).max(0.0);
    let bytes = (seconds * f64::from(rate.hz()) / 8.0) as u64;
    bytes.min(total_bytes)
}

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
    /// Kept so the native USB path can resolve the same DAC: Core Audio and USB report
    /// different names for one device, so only the user's own query matches both.
    pub query: Option<String>,
    /// Read once, because `device` goes stale while the DAC is held natively and the rates
    /// are a property of the DAC rather than of the `AudioDeviceID` it happens to have.
    dop_rates: Vec<u32>,
    /// DSD rates the device carries natively through Core Audio, which only a driver that
    /// owns the DAC's streaming interface can publish. Read once, for the same reason.
    native_rates: Vec<u32>,
    /// The DAC claimed for native DSD, held between tracks. Handing it back re-enumerates
    /// it, and the next claim would then have to race `usbaudiod` for a window that the
    /// re-enumeration has already closed.
    pub dac: Option<Held>,
    /// Set once the native path has been taken, because claiming a DAC re-enumerates it and
    /// that retires the `AudioDeviceID` `device` names.
    stale: bool,
}

impl Target {
    pub fn resolve(query: Option<&str>) -> Result<Self> {
        let (device, name) = output::find_device(query)?;
        Ok(Self {
            dop_rates: supported_dop_rates(&device),
            native_rates: supported_native_rates(&device),
            device,
            name,
            query: query.map(str::to_owned),
            dac: None,
            stale: false,
        })
    }

    /// True when this device advertises a PCM rate able to carry the file as DoP.
    fn carries_dop(&self, rate: DsdRate) -> bool {
        self.dop_rates.contains(&rate.dop_pcm_rate())
    }

    /// True when this device publishes the file's rate as native DSD through Core Audio.
    fn carries_native(&self, rate: DsdRate) -> bool {
        self.native_rates.contains(&rate.hz())
    }

    /// Hand a natively held DAC back to `usbaudiod`. Instant, because picking the device up
    /// again is left to whoever next needs it through Core Audio.
    pub fn release_dac(&mut self) {
        self.dac = None;
    }

    /// Hand back any native claim and point `device` at the DAC again, ready for Core Audio.
    ///
    /// Core Audio cannot see a device whose interfaces this process holds, and claiming one
    /// re-enumerates it, which retires the `AudioDeviceID` that `device` names. The device is
    /// looked up by name rather than by the user's query, because a query of `None` means the
    /// system default, which a device that has just come back is not yet.
    fn restore_core_audio(&mut self) -> Result<()> {
        self.release_dac();
        if !self.stale {
            return Ok(());
        }
        let name = self.name.clone();
        let deadline = Instant::now() + REATTACH_TIMEOUT;
        loop {
            // A device on its way out is still listed for a moment, but its streams are
            // already gone, so the rates it reports are what says it is really back.
            if let Ok((device, found)) = output::find_device(Some(&name))
                && !supported_dop_rates(&device).is_empty()
            {
                self.native_rates = supported_native_rates(&device);
                self.device = device;
                self.name = found;
                self.stale = false;
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("{name} has not come back to Core Audio since it was handed back");
            }
            thread::sleep(SETTLE);
        }
    }
}

#[derive(Debug, Default)]
struct FeedState {
    frames_written: AtomicU64,
    finished: AtomicBool,
    /// Set to park the reader, so a seek can take the source and drop the queue knowing
    /// nothing from the old position is still on its way.
    seeking: AtomicBool,
}

/// What the file being played contains.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub container: &'static str,
    pub tags: TrackTags,
    pub format: DsdFormat,
    pub duration: f64,
    pub total_frames: u64,
    pub bytes_per_channel: u64,
}

/// What the device settled on for this track.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    /// How the DSD reaches the DAC: "DoP" or "native DSD".
    pub carrier: &'static str,
    /// Container width the carrier uses: 24 bits for DoP, 32 for native DSD.
    pub bits: u8,
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
    /// Shared with the reader thread, which gives it up whenever a seek asks for it.
    source: Arc<Mutex<Box<dyn DsdSource>>>,
    feeder: Option<thread::JoinHandle<Result<()>>>,
    feed: Arc<FeedState>,
    stop: Arc<AtomicBool>,
    queue_frames: u64,
    /// DSD bytes per channel one device frame carries: two under DoP, four natively. Every
    /// frame count the session keeps is in device frames, so seeking converts through this.
    bytes_per_frame: u64,
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
        carrier: Carrier,
    ) -> Result<Self> {
        let source = reader::open(path)?;
        let format = source.format();
        let channels = format.channels as usize;
        // The queue holds DSD byte pairs whichever carrier takes them, so it is sized by the
        // DoP rate either way. The device runs at half that natively, where one frame takes
        // two pairs per channel instead of one.
        let payload_rate = format.rate.dop_pcm_rate();
        let frame_rate = payload_rate / carrier.payloads_per_frame() as u32;
        let bytes_per_frame = DOP_BYTES_PER_FRAME * carrier.payloads_per_frame() as u64;
        let track = TrackInfo {
            container: source.container(),
            tags: source.tags().clone(),
            format,
            duration: source.duration_secs(),
            total_frames: source.total_bytes_per_channel() / bytes_per_frame,
            bytes_per_channel: source.total_bytes_per_channel(),
        };

        let device = target.device;
        let capacity =
            (payload_rate as usize * channels * options.buffer_ms as usize / 1000).max(1 << 14);
        let (producer, consumer) = RingBuffer::<u16>::new(capacity);

        let request = Request {
            frame_rate,
            channels: format.channels,
            carrier,
            exclusive: options.exclusive,
            buffer_frames: options.buffer_frames,
        };
        let mut output = Output::open(device, &request, consumer)
            .with_context(|| format!("{} cannot play {}", target.name, format.rate))?;

        let integer = output.is_integer();
        let info = DeviceInfo {
            name: target.name.clone(),
            carrier: carrier.label(),
            bits: carrier.bits(),
            pcm_rate: frame_rate,
            buffer_frames: output.buffer_frames,
            transport: if integer { "integer" } else { "float32" },
            exclusive: output.is_exclusive(),
            mixing_disabled: output.mixing_disabled(),
            volume: (!integer).then(|| device.volume_scalar()).flatten(),
        };

        let feed = Arc::new(FeedState::default());
        let source = Arc::new(Mutex::new(source));
        let feeder = spawn_feeder(
            Arc::clone(&source),
            producer,
            Arc::clone(&feed),
            Arc::clone(stop),
            carrier,
        );

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
            source,
            feeder: Some(feeder),
            feed,
            stop: Arc::clone(stop),
            queue_frames: (capacity / (channels * carrier.payloads_per_frame())) as u64,
            bytes_per_frame,
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
        carrier: Carrier,
    ) -> Result<Self> {
        let mut last = None;
        for attempt in 1..=SETUP_ATTEMPTS {
            let error = match Self::open(path, target, options, stop, carrier) {
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

    /// Move the play position by `delta` seconds, clamped to the file.
    ///
    /// The callback sends DoP silence throughout, so the DAC keeps DSD lock across the jump
    /// and the seek costs it no relock.
    pub fn seek(&self, delta: f64) -> Result<()> {
        self.output.state.seeking.store(true, Ordering::Relaxed);
        self.feed.seeking.store(true, Ordering::Relaxed);
        let result = self.reposition(delta);
        self.feed.seeking.store(false, Ordering::Relaxed);
        self.await_prefill();
        self.output.state.seeking.store(false, Ordering::Relaxed);
        result
    }

    /// Take the source from the parked reader, move it, and drop the queue that was filled
    /// from the old position, moving the counters with it.
    fn reposition(&self, delta: f64) -> Result<()> {
        let target = seek_position(
            self.progress().elapsed,
            delta,
            self.track.format.rate,
            self.track.bytes_per_channel,
        );
        let mut source = self.source.lock().unwrap_or_else(PoisonError::into_inner);
        let reached = source.seek(target)?;
        self.output.state.drop_queued();
        let frames = reached / self.bytes_per_frame;
        self.output
            .state
            .frames_played
            .store(frames, Ordering::Relaxed);
        self.feed.frames_written.store(frames, Ordering::Relaxed);
        self.feed.finished.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Let the reader get ahead again before the queue is served, so a seek does not land as
    /// an underrun. Bounded, because a seek to the end of the file never fills the queue.
    fn await_prefill(&self) {
        let deadline = Instant::now() + PREFILL_TIMEOUT;
        while self.progress().queued_frames < self.queue_frames / 4
            && !self.feed.finished.load(Ordering::Relaxed)
            && !self.stop.load(Ordering::Relaxed)
            && Instant::now() < deadline
        {
            thread::sleep(PARK);
        }
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

/// One playing track, over whichever transport the device can carry it on.
pub enum Playback {
    /// Through Core Audio, over DoP or over a driver's native DSD format.
    HostAudio(Box<Session>),
    /// Through the USB interfaces this process claimed off `usbaudiod` itself.
    ClaimedUsb(Box<NativeSession>),
}

impl Playback {
    /// Open `path` on the best transport the target offers.
    ///
    /// DoP first, because it leaves the device an ordinary Core Audio device. A rate no PCM
    /// carrier reaches goes native, which is 32 DSD bits per frame instead of 16: through
    /// Core Audio when a driver owns the DAC and publishes the format, and otherwise by
    /// claiming the DAC's USB interfaces outright.
    pub fn open(
        path: &Path,
        target: &mut Target,
        options: &PlayOptions,
        stop: &Arc<AtomicBool>,
    ) -> Result<Self> {
        let rate = reader::open(path)?.format().rate;
        if target.carries_dop(rate) {
            // DoP goes through Core Audio, which cannot see a DAC this process is holding
            // for native DSD, so a claim carried over from an earlier track ends here.
            target.restore_core_audio()?;
            let session = Session::open_retrying(path, target, options, stop, Carrier::Dop)?;
            return Ok(Self::HostAudio(Box::new(session)));
        }
        // Both native paths take the mixer out of the way for the whole track and cannot be
        // shared. Say so rather than quietly doing the opposite of what was asked.
        if !options.exclusive {
            bail!(
                "{} cannot carry {rate} over DoP, and the native DSD path always claims the \
                 device exclusively; drop --shared to play this file",
                target.name
            );
        }
        if target.carries_native(rate) {
            // A driver already owns the DAC's streaming interface and publishes its
            // RAW_DATA alternate setting as a Core Audio format, so there is nothing to
            // claim: the bits go out through the HAL like any other non-mixable stream.
            target.restore_core_audio()?;
            let session = Session::open_retrying(path, target, options, stop, Carrier::NativeDsd)?;
            return Ok(Self::HostAudio(Box::new(session)));
        }
        if options.buffer_frames.is_some() {
            warn!("--buffer-frames sizes the Core Audio buffer, so it does not apply natively");
        }
        // Even a claim that fails may have re-enumerated the DAC, so Core Audio has to look
        // it up again before anything goes back over DoP.
        target.stale = true;
        let session = NativeSession::open(path, target, options.buffer_ms, stop)?;
        Ok(Self::ClaimedUsb(Box::new(session)))
    }

    pub fn track(&self) -> TrackInfo {
        match self {
            Self::HostAudio(session) => session.track.clone(),
            Self::ClaimedUsb(session) => session.track(),
        }
    }

    pub fn device(&self) -> DeviceInfo {
        match self {
            Self::HostAudio(session) => session.device.clone(),
            Self::ClaimedUsb(session) => session.device(),
        }
    }

    pub fn progress(&self) -> Progress {
        match self {
            Self::HostAudio(session) => session.progress(),
            Self::ClaimedUsb(session) => session.progress(),
        }
    }

    pub fn is_complete(&self) -> bool {
        match self {
            Self::HostAudio(session) => session.is_complete(),
            Self::ClaimedUsb(session) => session.is_complete(),
        }
    }

    /// True when playback stopped on its own, short of the end of the file. Only the claimed
    /// USB path can: a Core Audio IOProc runs until it is told to stop.
    pub fn has_stalled(&self) -> bool {
        match self {
            Self::HostAudio(_) => false,
            Self::ClaimedUsb(session) => session.has_stalled(),
        }
    }

    /// True once the reader has queued the whole file, after which silence is the tail
    /// rather than a dropout.
    pub fn fully_queued(&self) -> bool {
        match self {
            Self::HostAudio(session) => session.feed.finished.load(Ordering::Relaxed),
            Self::ClaimedUsb(session) => session.fully_queued(),
        }
    }

    pub fn set_paused(&self, paused: bool) {
        match self {
            Self::HostAudio(session) => session.set_paused(paused),
            Self::ClaimedUsb(session) => session.set_paused(paused),
        }
    }

    pub fn is_paused(&self) -> bool {
        match self {
            Self::HostAudio(session) => session.is_paused(),
            Self::ClaimedUsb(session) => session.is_paused(),
        }
    }

    /// Move the play position by `delta` seconds, clamped to the file.
    pub fn seek(&self, delta: f64) -> Result<()> {
        match self {
            Self::HostAudio(session) => session.seek(delta),
            Self::ClaimedUsb(session) => session.seek(delta),
        }
    }

    /// Stop playing and give the target its DAC back, so the next track finds it held.
    pub fn finish(self, target: &mut Target) -> Result<()> {
        match self {
            Self::HostAudio(session) => session.finish(),
            Self::ClaimedUsb(session) => {
                let (dac, result) = session.finish();
                target.dac = dac;
                result
            }
        }
    }
}

/// Play one file, printing progress until it ends or `stop` is set.
pub fn play(
    path: &Path,
    target: &mut Target,
    options: &PlayOptions,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let session = Playback::open(path, target, options, stop)?;
    let track = session.track();
    let device = session.device();
    warn_about_volume(&device);

    println!(
        "{}  {}  {}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        track.format,
        track.container
    );
    println!(
        "  -> {}: {} {} Hz, {} frame buffer, {}, {}{}",
        device.name,
        device.carrier,
        device.pcm_rate,
        device.buffer_frames,
        device.transport,
        if device.exclusive {
            "exclusive"
        } else {
            "shared"
        },
        if device.mixing_disabled {
            ", mixing off"
        } else {
            ""
        }
    );

    let duration = track.duration;
    let mut dropouts = 0;
    let mut shown = u64::MAX;
    let mut stalled = false;
    while !stop.load(Ordering::Relaxed) {
        let progress = session.progress();
        if session.is_complete() {
            break;
        }
        if session.has_stalled() {
            stalled = true;
            break;
        }
        if !session.fully_queued() {
            // Silence sent once the file is fully queued is the tail, not a dropout.
            dropouts = progress.underrun_frames;
        }
        if progress.elapsed as u64 != shown {
            shown = progress.elapsed as u64;
            print_progress(progress.elapsed, duration);
        }
        thread::sleep(POLL);
    }

    let frame_rate = device.pcm_rate;
    let elapsed = session.progress().elapsed;
    session.finish(target)?;
    print_progress(if stalled { elapsed } else { duration }, duration);
    println!();

    if stalled {
        bail!(
            "{}: stopped accepting transfers at {}; the track did not play out",
            device.name,
            clock(elapsed)
        );
    }

    if dropouts > 0 {
        eprintln!(
            "  {dropouts} frames of DSD silence filled underruns ({:.0} ms); raise --buffer-ms",
            dropouts as f64 * 1000.0 / f64::from(frame_rate)
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
    source: Arc<Mutex<Box<dyn DsdSource>>>,
    mut producer: Producer<u16>,
    feed: Arc<FeedState>,
    stop: Arc<AtomicBool>,
    carrier: Carrier,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || {
        let (channels, chunk_bytes) = {
            let source = source.lock().unwrap_or_else(PoisonError::into_inner);
            (source.format().channels as usize, source.chunk_bytes())
        };
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; chunk_bytes].into(); channels];
        let mut payloads = Vec::new();
        let mut queued = Queued::new(channels * carrier.payloads_per_frame());
        let result = feed_loop(
            &source,
            &mut producer,
            &feed,
            &stop,
            (&mut planes, &mut payloads, &mut queued),
        );
        feed.finished.store(true, Ordering::Relaxed);
        result
    })
}

/// The reader's running count of what it has handed the queue.
///
/// Payloads are counted rather than frames because a native frame takes two of them per
/// channel, and a chunk of DSD does not have to end on one: flooring each chunk on its own
/// would lose a frame here and there, and the transport reads the difference between what is
/// queued and what is played as the queue depth.
struct Queued {
    frame_payloads: usize,
    pending: usize,
}

impl Queued {
    const fn new(frame_payloads: usize) -> Self {
        Self {
            frame_payloads,
            pending: 0,
        }
    }

    fn advance(&mut self, feed: &FeedState, payloads: usize) {
        if self.frame_payloads == 0 {
            return;
        }
        self.pending += payloads;
        let frames = self.pending / self.frame_payloads;
        self.pending -= frames * self.frame_payloads;
        feed.frames_written
            .fetch_add(frames as u64, Ordering::Relaxed);
    }
}

/// Read, pack, and queue until the file ends or `stop` is set.
///
/// The source stays locked for as long as a chunk is in flight, so a seek that takes it back
/// knows nothing read from the old position is still on its way to the queue. Both waits give
/// it up as soon as a seek asks.
fn feed_loop(
    source: &Mutex<Box<dyn DsdSource>>,
    producer: &mut Producer<u16>,
    feed: &FeedState,
    stop: &AtomicBool,
    reading: (&mut [Box<[u8]>], &mut Vec<u16>, &mut Queued),
) -> Result<()> {
    let (planes, payloads, queued) = reading;
    while !stop.load(Ordering::Relaxed) {
        if feed.seeking.load(Ordering::Relaxed) {
            // A seek resets the counters, so a part frame left from the old position would
            // be counted against the new one.
            queued.pending = 0;
            thread::sleep(PARK);
            continue;
        }
        let mut source = source.lock().unwrap_or_else(PoisonError::into_inner);
        let count = source.read(planes)?;
        if count == 0 {
            // The end of the file, not the end of the thread: a seek back into the track
            // still has to find a reader.
            feed.finished.store(true, Ordering::Relaxed);
            drop(source);
            thread::sleep(PARK);
            continue;
        }
        payloads.clear();
        let slices: Vec<&[u8]> = planes.iter().map(|plane| &plane[..count]).collect();
        dop::pack_planes(&slices, payloads);
        queue(producer, feed, stop, payloads, queued)?;
    }
    Ok(())
}

/// Hand `payloads` to the ring, waiting for room. A stop or a seek gives up on whatever is
/// left, which the queue it would have joined is dropping anyway.
fn queue(
    producer: &mut Producer<u16>,
    feed: &FeedState,
    stop: &AtomicBool,
    payloads: &[u16],
    queued: &mut Queued,
) -> Result<()> {
    let mut offset = 0;
    while offset < payloads.len() {
        if stop.load(Ordering::Relaxed) || feed.seeking.load(Ordering::Relaxed) {
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
        queued.advance(feed, take);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::dsd::DsdRate;
    use crate::player::seek_position;

    /// One second of DSD64 is 2 822 400 bits, so 352 800 bytes per channel.
    const SECOND: u64 = 352_800;

    #[test]
    fn a_seek_forward_lands_a_whole_number_of_seconds_on() {
        let reached = seek_position(10.0, 5.0, DsdRate::new(2_822_400), SECOND * 60);

        assert_eq!(reached, SECOND * 15);
    }

    #[test]
    fn a_seek_back_past_the_start_lands_on_the_start() {
        let reached = seek_position(2.0, -5.0, DsdRate::new(2_822_400), SECOND * 60);

        assert_eq!(reached, 0);
    }

    #[test]
    fn a_seek_forward_past_the_end_lands_on_the_end() {
        let reached = seek_position(59.0, 5.0, DsdRate::new(2_822_400), SECOND * 60);

        assert_eq!(reached, SECOND * 60);
    }
}
