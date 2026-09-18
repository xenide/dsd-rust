use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::AudioFormat;
use crate::dsd::DsdRate;
use crate::output::hal::Volume;
use crate::output::stream::{
    Carrier, DOP_PCM_RATES, DeviceBusy, Output, Request, probe_dop_rate, supported_dop_rates,
};
use crate::output::usb::device::{Dac, Held};
use crate::output::usb::session::NativeSession;
use crate::output::{self, hal::Device};
use crate::reader::tags::TrackTags;
use crate::reader::{self, Source, TrackRef};
use anyhow::{Context, Result, bail};
use rtrb::{Producer, RingBuffer};
use tracing::{debug, warn};

/// How long the DAC keeps receiving silence after the music ends, so it does not pop.
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

/// Where `delta` seconds from `elapsed` lands, in whatever unit `per_second` counts, clamped
/// to `total`. Carrier frames for a Core Audio session, DSD bytes for a native one.
pub(crate) fn seek_to(elapsed: f64, delta: f64, per_second: f64, total: u64) -> u64 {
    let seconds = (elapsed + delta).max(0.0);
    ((seconds * per_second) as u64).min(total)
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
    /// DoP carrier rates the DAC's own clock claims that Core Audio does not advertise,
    /// waiting to be probed. A rate leaves this list the first time it is tried, so one the
    /// device turns down does not cost a probe again on every track.
    unprobed_rates: Vec<u32>,
    /// Unadvertised rates a probe proved the device accepts, which the stream choice has to
    /// be told about because they are in no format list it can read.
    probed_rates: Vec<u32>,
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
        let dop_rates = supported_dop_rates(&device);
        Ok(Self {
            unprobed_rates: unadvertised_dop_rates(&name, &dop_rates),
            probed_rates: Vec::new(),
            dop_rates,
            device,
            name,
            query: query.map(str::to_owned),
            dac: None,
            stale: false,
        })
    }

    /// True when this device can carry the file as DoP.
    ///
    /// A rate Core Audio does not advertise is not one the DAC has refused: the advertised
    /// list is an intersection that is sometimes narrower than the hardware. Where the DAC's
    /// clock claims the rate regardless, setting it and reading it back settles the question,
    /// and a DAC that answers yes skips the native path and the interface race behind it.
    fn carries_dop(&mut self, rate: DsdRate) -> Result<bool> {
        let pcm_rate = rate.dop_pcm_rate();
        if self.dop_rates.contains(&pcm_rate) {
            return Ok(true);
        }
        if !self.unprobed_rates.contains(&pcm_rate) {
            return Ok(false);
        }
        self.unprobed_rates.retain(|unprobed| *unprobed != pcm_rate);
        // The probe goes through Core Audio, which cannot see a DAC this process is holding
        // for native DSD, so a claim carried over from an earlier track ends before it.
        self.restore_core_audio()?;
        if !probe_dop_rate(&self.device, pcm_rate) {
            return Ok(false);
        }
        debug!(
            "{}: takes an unadvertised {pcm_rate} Hz DoP carrier",
            self.name
        );
        self.probed_rates.push(pcm_rate);
        Ok(true)
    }

    /// Where the device's own volume control sits, when macOS can see it and it has one.
    ///
    /// A DAC held for native DSD has left Core Audio, and one that attenuates nowhere but in
    /// its own analogue stage has no control to read.
    pub fn volume(&self) -> Option<Volume> {
        if self.stale {
            return None;
        }
        self.device.volume()
    }

    /// Move the device's volume by `decibels`.
    pub fn adjust_volume(&self, decibels: f32) -> Result<Volume> {
        if self.stale {
            bail!(
                "{} is claimed for native DSD, so macOS has no volume control over it",
                self.name
            );
        }
        self.device
            .adjust_volume(decibels)
            .with_context(|| format!("{} volume", self.name))
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

/// DoP carrier rates the DAC's clock reports that Core Audio does not advertise.
///
/// `kAudioStreamPropertyAvailablePhysicalFormats` is derived from the intersection of the
/// streaming alternate settings with the clock ranges, and a DAC that lists no PCM alternate
/// setting at a rate its clock reaches drops out of that intersection. The clock's own RANGE
/// report is the second opinion, and the only rates worth the cost of a probe are the ones
/// the two disagree on.
fn unadvertised_dop_rates(name: &str, advertised: &[u32]) -> Vec<u32> {
    let Ok(dacs) = Dac::discover() else {
        return Vec::new();
    };
    let Some(dac) = dacs.iter().find(|dac| dac.matches(name)) else {
        return Vec::new();
    };
    let clock = match dac.clock_rates() {
        Ok(clock) => clock,
        Err(error) => {
            debug!("{name}: cannot read the clock range, so no rate is probed: {error}");
            return Vec::new();
        }
    };
    let mut rates = Vec::new();
    for rate in DOP_PCM_RATES {
        if clock.contains(&rate) && !advertised.contains(&rate) {
            rates.push(rate);
        }
    }
    rates
}

#[derive(Debug, Default)]
struct FeedState {
    frames_written: AtomicU64,
    finished: AtomicBool,
    /// Set to park the reader, so a seek can take the source and drop the queue knowing
    /// nothing from the old position is still on its way.
    seeking: AtomicBool,
}

/// What the recording being played contains.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub container: &'static str,
    pub tags: TrackTags,
    pub format: AudioFormat,
    pub duration: f64,
    /// Carrier frames the whole recording holds.
    pub total_frames: u64,
}

/// What the device settled on for this track.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    /// How the audio reaches the DAC: "DoP", "native DSD", or "PCM".
    pub carrier: &'static str,
    /// Width of one sample as the carrier presents it.
    pub bits: u32,
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

/// One track playing to one device: the reader thread, the carrier queue, and the callback.
pub struct Session {
    output: Output,
    /// Shared with the reader thread, which gives it up whenever a seek asks for it.
    source: Arc<Mutex<Source>>,
    feeder: Option<thread::JoinHandle<Result<()>>>,
    feed: Arc<FeedState>,
    stop: Arc<AtomicBool>,
    queue_frames: u64,
    pub track: TrackInfo,
    pub device: DeviceInfo,
}

impl Session {
    /// Open the recording, take over the device, prefill the queue, and start the callback.
    pub fn open(
        track: &TrackRef,
        target: &Target,
        options: &PlayOptions,
        stop: &Arc<AtomicBool>,
    ) -> Result<Self> {
        let source = reader::open(track)?;
        let format = source.format();
        let channels = format.channels() as usize;
        let pcm_rate = format.carrier_rate();
        let info = TrackInfo {
            container: source.container(),
            tags: source.tags().clone(),
            format,
            duration: source.duration_secs(),
            total_frames: source.total_frames(),
        };

        let device = target.device;
        // Room for the queue the reader fills, and never less than a few of its chunks: a
        // chunk that cannot fit would leave the reader waiting for room that never comes.
        let capacity = (pcm_rate as usize * channels * options.buffer_ms as usize / 1000)
            .max(1 << 14)
            .max(source.chunk_frames() * channels * 4);
        let (producer, consumer) = RingBuffer::<i32>::new(capacity);

        let request = Request {
            pcm_rate,
            channels: format.channels(),
            carrier: match format {
                AudioFormat::Dsd(_) => Carrier::Dop,
                AudioFormat::Pcm(_) => Carrier::Pcm,
            },
            exclusive: options.exclusive,
            buffer_frames: options.buffer_frames,
            allow_unadvertised_rate: target.probed_rates.contains(&pcm_rate),
        };
        let mut output = Output::open(device, &request, consumer)
            .with_context(|| format!("{} cannot play {format}", target.name))?;

        let device = DeviceInfo {
            name: target.name.clone(),
            carrier: format.carrier_name(),
            bits: format.carrier_bits(),
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
                .then(|| target.device.volume_scalar())
                .flatten(),
        };

        let feed = Arc::new(FeedState::default());
        let source = Arc::new(Mutex::new(source));
        let feeder = spawn_feeder(
            Arc::clone(&source),
            producer,
            Arc::clone(&feed),
            Arc::clone(stop),
        );

        let prefill = (capacity / channels / 2) as u64;
        while feed.frames_written.load(Ordering::Relaxed) < prefill.min(info.total_frames)
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
            queue_frames: (capacity / channels) as u64,
            track: info,
            device,
        })
    }

    /// Open a track, giving a device that is still settling from the previous one more goes.
    pub fn open_retrying(
        track: &TrackRef,
        target: &Target,
        options: &PlayOptions,
        stop: &Arc<AtomicBool>,
    ) -> Result<Self> {
        let mut last = None;
        for attempt in 1..=SETUP_ATTEMPTS {
            let error = match Self::open(track, target, options, stop) {
                Ok(session) => return Ok(session),
                Err(error) => error,
            };
            if attempt == SETUP_ATTEMPTS || error.downcast_ref::<DeviceBusy>().is_none() {
                return Err(error);
            }
            debug!("{error}; retrying {track}");
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

    /// True once the whole recording has been queued and the callback has consumed all of it.
    pub fn is_complete(&self) -> bool {
        self.feed.finished.load(Ordering::Relaxed)
            && self.output.state.frames_played.load(Ordering::Relaxed)
                >= self.feed.frames_written.load(Ordering::Relaxed)
    }

    /// Hold the queue where it is. The callback keeps sending carrier silence, so a DSD
    /// stream keeps its lock and resuming does not cost the DAC a relock.
    pub fn set_paused(&self, paused: bool) {
        self.output.state.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.output.state.paused.load(Ordering::Relaxed)
    }

    /// Move the play position by `delta` seconds, clamped to the recording.
    ///
    /// The callback sends silence throughout, so the DAC keeps lock across the jump and the
    /// seek costs it no relock.
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
        let target = seek_to(
            self.progress().elapsed,
            delta,
            f64::from(self.device.pcm_rate),
            self.track.total_frames,
        );
        let mut source = self.source.lock().unwrap_or_else(PoisonError::into_inner);
        let frames = source.seek(target)?;
        self.output.state.drop_queued();
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
    CoreAudio(Box<Session>),
    Native(Box<NativeSession>),
}

impl Playback {
    /// Open `track`. PCM and DSD alike go through Core Audio at the rate the recording
    /// wants; only a DAC whose PCM rates cannot carry a DSD file as DoP takes the native
    /// path, where 32 DSD bits ride in each frame instead of 16.
    pub fn open(
        track: &TrackRef,
        target: &mut Target,
        options: &PlayOptions,
        stop: &Arc<AtomicBool>,
    ) -> Result<Self> {
        let format = reader::open(track)?.format();
        let carried_as_dop = match format.dsd() {
            // PCM is carried by locking the device to the file's own sample rate, which every
            // DAC that plays the rate at all can do.
            None => true,
            Some(dsd) => target.carries_dop(dsd.rate)?,
        };
        if carried_as_dop {
            // Core Audio cannot see a DAC this process is holding for native DSD, so a claim
            // carried over from an earlier track ends here.
            target.restore_core_audio()?;
            let session = Session::open_retrying(track, target, options, stop)?;
            return Ok(Self::CoreAudio(Box::new(session)));
        }
        // The native path takes both audio interfaces away from usbaudiod for the whole
        // track, which is heavier than hog mode and cannot be shared. Say so rather than
        // quietly doing the opposite of what was asked.
        if !options.exclusive {
            bail!(
                "{} cannot carry {format} over DoP, and the native DSD path always claims the \
                 device exclusively; drop --shared to play this file",
                target.name
            );
        }
        if options.buffer_frames.is_some() {
            warn!("--buffer-frames sizes the Core Audio buffer, so it does not apply natively");
        }
        // Even a claim that fails may have re-enumerated the DAC, so Core Audio has to look
        // it up again before anything goes back over DoP.
        target.stale = true;
        let session = NativeSession::open(track, target, options.buffer_ms, stop)?;
        Ok(Self::Native(Box::new(session)))
    }

    pub fn track(&self) -> TrackInfo {
        match self {
            Self::CoreAudio(session) => session.track.clone(),
            Self::Native(session) => session.track(),
        }
    }

    pub fn device(&self) -> DeviceInfo {
        match self {
            Self::CoreAudio(session) => session.device.clone(),
            Self::Native(session) => session.device(),
        }
    }

    pub fn progress(&self) -> Progress {
        match self {
            Self::CoreAudio(session) => session.progress(),
            Self::Native(session) => session.progress(),
        }
    }

    pub fn is_complete(&self) -> bool {
        match self {
            Self::CoreAudio(session) => session.is_complete(),
            Self::Native(session) => session.is_complete(),
        }
    }

    /// True when playback stopped on its own, short of the end of the file. Only the native
    /// path can: a Core Audio IOProc runs until it is told to stop.
    pub fn has_stalled(&self) -> bool {
        match self {
            Self::CoreAudio(_) => false,
            Self::Native(session) => session.has_stalled(),
        }
    }

    /// True once the reader has queued the whole recording, after which silence is the tail
    /// rather than a dropout.
    pub fn fully_queued(&self) -> bool {
        match self {
            Self::CoreAudio(session) => session.feed.finished.load(Ordering::Relaxed),
            Self::Native(session) => session.fully_queued(),
        }
    }

    pub fn set_paused(&self, paused: bool) {
        match self {
            Self::CoreAudio(session) => session.set_paused(paused),
            Self::Native(session) => session.set_paused(paused),
        }
    }

    pub fn is_paused(&self) -> bool {
        match self {
            Self::CoreAudio(session) => session.is_paused(),
            Self::Native(session) => session.is_paused(),
        }
    }

    /// Move the play position by `delta` seconds, clamped to the recording.
    pub fn seek(&self, delta: f64) -> Result<()> {
        match self {
            Self::CoreAudio(session) => session.seek(delta),
            Self::Native(session) => session.seek(delta),
        }
    }

    /// Stop playing and give the target its DAC back, so the next track finds it held.
    pub fn finish(self, target: &mut Target) -> Result<()> {
        match self {
            Self::CoreAudio(session) => session.finish(),
            Self::Native(session) => {
                let (dac, result) = session.finish();
                target.dac = dac;
                result
            }
        }
    }
}

/// Play one recording, printing progress until it ends or `stop` is set.
pub fn play(
    track: &TrackRef,
    target: &mut Target,
    options: &PlayOptions,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let session = Playback::open(track, target, options, stop)?;
    let info = session.track();
    let device = session.device();
    warn_about_volume(&device);

    println!("{}  {}  {}", track.label(), info.format, info.container);
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

    let duration = info.duration;
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
            "  {dropouts} frames of silence filled underruns ({:.0} ms); raise --buffer-ms",
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

/// A software volume scales the samples, which destroys the DoP markers and quietly changes
/// PCM. Integer transport has no gain stage at all, and a DAC that applies its own volume
/// never sees one either, so the warning is only worth making when the samples pass through
/// Core Audio as float.
fn warn_about_volume(device: &DeviceInfo) {
    let Some(volume) = device.volume else {
        return;
    };
    if volume < 1.0 {
        eprintln!(
            "warning: device volume is {:.0}%. If macOS applies it in software the samples no \
             longer reach the DAC untouched; if the DAC applies it itself, playback is \
             bit-perfect all the same.",
            volume * 100.0
        );
    }
}

fn spawn_feeder(
    source: Arc<Mutex<Source>>,
    mut producer: Producer<i32>,
    feed: Arc<FeedState>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || {
        let channels = {
            let source = source.lock().unwrap_or_else(PoisonError::into_inner);
            source.format().channels() as usize
        };
        let mut carried = Vec::new();
        let result = feed_loop(&source, &mut producer, &feed, &stop, channels, &mut carried);
        feed.finished.store(true, Ordering::Relaxed);
        result
    })
}

/// Read, pack, and queue until the recording ends or `stop` is set.
///
/// The source stays locked for as long as a chunk is in flight, so a seek that takes it back
/// knows nothing read from the old position is still on its way to the queue. Both waits give
/// it up as soon as a seek asks.
fn feed_loop(
    source: &Mutex<Source>,
    producer: &mut Producer<i32>,
    feed: &FeedState,
    stop: &AtomicBool,
    channels: usize,
    carried: &mut Vec<i32>,
) -> Result<()> {
    while !stop.load(Ordering::Relaxed) {
        if feed.seeking.load(Ordering::Relaxed) {
            thread::sleep(PARK);
            continue;
        }
        let mut source = source.lock().unwrap_or_else(PoisonError::into_inner);
        carried.clear();
        let count = source.read(carried)?;
        if count == 0 {
            // The end of the recording, not the end of the thread: a seek back into the
            // track still has to find a reader.
            feed.finished.store(true, Ordering::Relaxed);
            drop(source);
            thread::sleep(PARK);
            continue;
        }
        queue(producer, feed, stop, carried, channels)?;
    }
    Ok(())
}

/// Hand `samples` to the ring, waiting for room. A stop or a seek gives up on whatever is
/// left, which the queue it would have joined is dropping anyway.
fn queue(
    producer: &mut Producer<i32>,
    feed: &FeedState,
    stop: &AtomicBool,
    samples: &[i32],
    channels: usize,
) -> Result<()> {
    let mut offset = 0;
    while offset < samples.len() {
        if stop.load(Ordering::Relaxed) || feed.seeking.load(Ordering::Relaxed) {
            return Ok(());
        }
        let free = producer.slots();
        if free == 0 {
            thread::sleep(Duration::from_millis(2));
            continue;
        }
        let take = free.min(samples.len() - offset);
        let chunk = producer.write_chunk_uninit(take)?;
        chunk.fill_from_iter(samples[offset..offset + take].iter().copied());
        offset += take;
        feed.frames_written
            .fetch_add((take / channels) as u64, Ordering::Relaxed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::player::seek_to;

    /// One second of DSD64 carried as DoP is 176 400 frames.
    const DOP_SECOND: f64 = 176_400.0;

    #[test]
    fn a_seek_forward_lands_a_whole_number_of_seconds_on() {
        let reached = seek_to(10.0, 5.0, DOP_SECOND, 176_400 * 60);

        assert_eq!(reached, 176_400 * 15);
    }

    #[test]
    fn a_seek_back_past_the_start_lands_on_the_start() {
        let reached = seek_to(2.0, -5.0, DOP_SECOND, 176_400 * 60);

        assert_eq!(reached, 0);
    }

    #[test]
    fn a_seek_forward_past_the_end_lands_on_the_end() {
        let reached = seek_to(59.0, 5.0, DOP_SECOND, 176_400 * 60);

        assert_eq!(reached, 176_400 * 60);
    }

    #[test]
    fn a_pcm_seek_counts_in_the_files_own_sample_rate() {
        let reached = seek_to(1.0, 5.0, 96_000.0, 96_000 * 60);

        assert_eq!(reached, 96_000 * 6);
    }
}
