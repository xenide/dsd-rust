use std::cmp::Reverse;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use coreaudio_sys::{
    AudioBufferList, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioObjectID, AudioStreamBasicDescription,
    AudioStreamRangedDescription, AudioTimeStamp, OSStatus, kAudioFormatFlagIsNonMixable,
    kAudioFormatLinearPCM,
};
use rtrb::Consumer;
use tracing::{debug, warn};

use crate::dop::{self, Marker, SILENCE_PAYLOAD};
use crate::dsd::DsdRate;
use crate::native::BYTES_PER_SUBSLOT;
use crate::output::FormatLine;
use crate::output::encoding::{Carrier, Encoding, is_native_dsd};
use crate::output::hal::{self, Device, Stream};

/// Counters the render callback publishes for the transport display.
#[derive(Debug, Default)]
pub struct PlaybackState {
    pub frames_played: AtomicU64,
    pub underrun_frames: AtomicU64,
    /// Set to drop whatever is queued and send DoP silence, so playback can end without a pop.
    pub silence: AtomicBool,
    /// Set to hold the queue where it is and send DoP silence, so the DAC keeps DSD lock.
    pub paused: AtomicBool,
    /// Set while a seek is being served: the queue is left alone and a short read is not an
    /// underrun, because the reader is repositioning rather than falling behind.
    pub seeking: AtomicBool,
    /// Set to drop everything queued, which the render path does and then clears. Only ever
    /// set while the reader is parked, so one pass empties the queue.
    pub flush: AtomicBool,
}

/// How long a seek waits for the render path to drop what is queued.
const FLUSH_TIMEOUT: Duration = Duration::from_millis(200);

impl PlaybackState {
    /// Drop whatever is queued and wait for it, because only the render path owns the
    /// consuming end. A render path that has already stopped never clears the flag, so the
    /// wait is bounded: a moment of stale audio beats a transport that will not move.
    pub fn drop_queued(&self) {
        self.flush.store(true, Ordering::Release);
        let deadline = Instant::now() + FLUSH_TIMEOUT;
        while self.flush.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                self.flush.store(false, Ordering::Release);
                warn!("the render path did not drop the queue for a seek");
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Payloads one native frame takes from the queue at the widest file the readers accept:
/// two DSD byte pairs for each of six channels.
const MAX_NATIVE_PAYLOADS: usize = 12;

struct IoContext {
    consumer: Consumer<u16>,
    channels: usize,
    stream_index: usize,
    carrier: Carrier,
    /// How a DoP word sits in one sample of the stream format. Native DSD writes its bytes
    /// straight out, so it never consults this.
    encoding: Encoding,
    marker: Marker,
    state: Arc<PlaybackState>,
}

impl IoContext {
    fn fill(&mut self, out: &mut [u8], stream_channels: usize) {
        let Self {
            consumer,
            channels,
            carrier,
            encoding,
            marker,
            state,
            ..
        } = self;
        if state.flush.swap(false, Ordering::AcqRel) {
            let queued = consumer.slots();
            if let Ok(stale) = consumer.read_chunk(queued) {
                stale.commit_all();
            }
        }

        let sample_bytes = match carrier {
            Carrier::Dop => encoding.bytes_per_sample(),
            Carrier::NativeDsd => BYTES_PER_SUBSLOT,
        };
        let frame_bytes = sample_bytes * stream_channels;
        if frame_bytes == 0 || stream_channels == 0 {
            return;
        }
        let frames = out.len() / frame_bytes;
        // Native DSD packs twice the DSD bits into a frame, so one frame drains two payloads
        // per channel rather than one. Everything else about the queue is the same.
        let per_frame = *channels * carrier.payloads_per_frame();
        let silenced = state.silence.load(Ordering::Relaxed)
            || state.paused.load(Ordering::Relaxed)
            || state.seeking.load(Ordering::Relaxed);
        let ready = if silenced || per_frame == 0 {
            0
        } else {
            (consumer.slots() / per_frame).min(frames)
        };
        let chunk = consumer
            .read_chunk(ready * per_frame)
            .expect("counted slots are available");
        let (head, tail) = chunk.as_slices();
        let mut payloads = head.iter().chain(tail).copied();

        match carrier {
            Carrier::Dop => write_dop(
                &mut payloads,
                *encoding,
                marker,
                *channels,
                stream_channels,
                frames,
                out,
            ),
            Carrier::NativeDsd => {
                write_native(&mut payloads, *channels, stream_channels, frames, out);
            }
        }
        chunk.commit_all();

        // Counted in device frames, which is what the queue is measured in too: a native
        // frame carries two payloads per channel, and the reader accounts for it the same way.
        state
            .frames_played
            .fetch_add(ready as u64, Ordering::Relaxed);
        if !silenced {
            state
                .underrun_frames
                .fetch_add((frames - ready) as u64, Ordering::Relaxed);
        }
    }
}

/// Write `frames` DoP frames: an alternating marker and 16 DSD bits per channel.
fn write_dop(
    payloads: &mut impl Iterator<Item = u16>,
    encoding: Encoding,
    marker: &mut Marker,
    channels: usize,
    stream_channels: usize,
    frames: usize,
    out: &mut [u8],
) {
    let sample_bytes = encoding.bytes_per_sample();
    let mut cursor = 0;
    for _ in 0..frames {
        let marker = marker.next();
        for channel in 0..stream_channels {
            let payload = if channel < channels {
                payloads.next().unwrap_or(SILENCE_PAYLOAD)
            } else {
                SILENCE_PAYLOAD
            };
            encoding.write(
                dop::word(marker, payload),
                &mut out[cursor..cursor + sample_bytes],
            );
            cursor += sample_bytes;
        }
    }
}

/// Write `frames` native frames: four raw DSD bytes per channel, earliest byte first.
///
/// The queue interleaves one payload per channel per DoP frame, so a native frame's two
/// payloads for one channel sit `channels` apart rather than side by side. They are staged
/// for the whole frame first, because the iterator only moves forward.
fn write_native(
    payloads: &mut impl Iterator<Item = u16>,
    channels: usize,
    stream_channels: usize,
    frames: usize,
    out: &mut [u8],
) {
    let mut staged = [SILENCE_PAYLOAD; MAX_NATIVE_PAYLOADS];
    let wanted = (channels * 2).min(MAX_NATIVE_PAYLOADS);
    let mut cursor = 0;
    for _ in 0..frames {
        for slot in staged.iter_mut().take(wanted) {
            *slot = payloads.next().unwrap_or(SILENCE_PAYLOAD);
        }
        for channel in 0..stream_channels {
            // A channel the file does not carry gets DSD silence, as it does under DoP.
            let (early, late) = if channel < channels {
                (
                    staged.get(channel).copied().unwrap_or(SILENCE_PAYLOAD),
                    staged
                        .get(channels + channel)
                        .copied()
                        .unwrap_or(SILENCE_PAYLOAD),
                )
            } else {
                (SILENCE_PAYLOAD, SILENCE_PAYLOAD)
            };
            out[cursor..cursor + 2].copy_from_slice(&early.to_be_bytes());
            out[cursor + 2..cursor + BYTES_PER_SUBSLOT].copy_from_slice(&late.to_be_bytes());
            cursor += BYTES_PER_SUBSLOT;
        }
    }
}

unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    _input: *const AudioBufferList,
    _input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> OSStatus {
    if client.is_null() || output.is_null() {
        return 0;
    }
    let context = unsafe { &mut *client.cast::<IoContext>() };
    let list = unsafe { &mut *output };
    let buffers = unsafe {
        std::slice::from_raw_parts_mut(list.mBuffers.as_mut_ptr(), list.mNumberBuffers as usize)
    };

    for (index, buffer) in buffers.iter_mut().enumerate() {
        let bytes = buffer.mDataByteSize as usize;
        if buffer.mData.is_null() || bytes == 0 {
            continue;
        }
        let data = unsafe { std::slice::from_raw_parts_mut(buffer.mData.cast::<u8>(), bytes) };
        if index == context.stream_index {
            context.fill(data, buffer.mNumberChannels as usize);
        } else {
            data.fill(0);
        }
    }
    0
}

/// How long a device gets to become startable after another track released it.
const START_TIMEOUT: Duration = Duration::from_secs(2);

/// Core Audio reports a device that is still reconfiguring as temporarily unavailable.
const EAGAIN: OSStatus = 35;

/// The device is mid-reconfiguration; the same request usually succeeds a moment later.
#[derive(Debug, thiserror::Error)]
#[error("the audio device is busy reconfiguring")]
pub struct DeviceBusy;

/// How long a device gets to adopt a stream format.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(3);

/// How long the virtual format gets to follow a non-mixable physical format.
const VIRTUAL_FORMAT_TIMEOUT: Duration = Duration::from_millis(500);

/// The DSD rates a DoP link can carry, as PCM frame rates.
pub const DOP_PCM_RATES: [u32; 4] = [176_400, 352_800, 705_600, 1_411_200];

fn carries_rate(ranged: &AudioStreamRangedDescription, rate: u32) -> bool {
    let wanted = f64::from(rate);
    if ranged.mFormat.mSampleRate == wanted {
        return true;
    }
    ranged.mFormat.mSampleRate == 0.0
        && ranged.mSampleRateRange.mMinimum <= wanted
        && wanted <= ranged.mSampleRateRange.mMaximum
}

/// A non-mixable format takes the Core Audio mixer out of the path, which is what makes
/// the callback receive the device's own integer samples instead of converted float.
fn is_non_mixable(format: &AudioStreamBasicDescription) -> bool {
    format.mFormatFlags & kAudioFormatFlagIsNonMixable != 0
}

/// Prefer a non-mixable integer format, then 24-bit containers, then an exact channel match.
fn score(
    format: &AudioStreamBasicDescription,
    encoding: Encoding,
    channels: u32,
    exclusive: bool,
) -> i32 {
    let mut score = 0;
    if exclusive && is_non_mixable(format) {
        score += 200;
    }
    if encoding.is_integer() {
        score += 100;
    }
    if format.mBitsPerChannel == 24 {
        score += 20;
    }
    if format.mChannelsPerFrame == channels {
        score += 10;
    }
    score
}

/// PCM rates from the DSD family that this device can carry with at least 24 bits.
pub fn supported_dop_rates(device: &Device) -> Vec<u32> {
    let mut rates = Vec::new();
    let Ok(streams) = device.output_streams() else {
        return rates;
    };
    for stream in streams {
        let Ok(formats) = stream.available_physical_formats() else {
            continue;
        };
        for rate in DOP_PCM_RATES {
            let usable = formats.iter().any(|ranged| {
                carries_rate(ranged, rate)
                    && ranged.mFormat.mFormatID == kAudioFormatLinearPCM
                    // A native DSD format is 32-bit integer PCM as far as this test can
                    // tell, and DoP words written into it reach the DAC as raw DSD bits.
                    && !is_native_dsd(&ranged.mFormat)
                    && Encoding::from_format(&ranged.mFormat).is_ok()
            });
            if usable && !rates.contains(&rate) {
                rates.push(rate);
            }
        }
    }
    rates.sort_unstable();
    rates
}

/// DSD rates this device carries natively through Core Audio, in DSD bits per second.
///
/// Only a driver that owns the DAC's streaming interface can publish these: Core Audio never
/// selects a `RAW_DATA` alternate setting on its own, so a DAC left to `usbaudiod` offers
/// nothing here however willing the hardware is. The frame rate is a thirty-second of the DSD
/// rate, and a frame rate counts only when that lands on a real DSD tier, which is what
/// separates 352800 Hz carrying DSD256 from 352800 Hz carrying PCM.
pub fn supported_native_rates(device: &Device) -> Vec<u32> {
    let mut rates = Vec::new();
    let Ok(streams) = device.output_streams() else {
        return rates;
    };
    for stream in streams {
        let Ok(formats) = stream.available_physical_formats() else {
            continue;
        };
        for ranged in formats {
            if !is_native_dsd(&ranged.mFormat) || ranged.mFormat.mSampleRate <= 0.0 {
                continue;
            }
            let hz = (ranged.mFormat.mSampleRate as u32).saturating_mul(32);
            if DsdRate::new(hz).multiplier().is_some_and(|n| n >= 64) && !rates.contains(&hz) {
                rates.push(hz);
            }
        }
    }
    rates.sort_unstable();
    rates
}

/// What the player asks of a device.
pub struct Request {
    /// Frames a second the device runs at, which is the DoP carrier rate or, natively, a
    /// thirty-second of the DSD rate.
    pub frame_rate: u32,
    pub channels: u16,
    pub carrier: Carrier,
    pub exclusive: bool,
    pub buffer_frames: Option<u32>,
}

/// A running DoP output: device settings taken over on open and restored on drop.
pub struct Output {
    device: Device,
    stream: Stream,
    proc_id: AudioDeviceIOProcID,
    context: *mut IoContext,
    original_format: AudioStreamBasicDescription,
    original_rate: f64,
    mixing_disabled: bool,
    hogged: bool,
    running: bool,
    format_changed: bool,
    /// Set when this device was the system output before exclusive use moved it away.
    was_default_output: bool,
    pub carrier: Carrier,
    pub encoding: Encoding,
    pub buffer_frames: u32,
    pub state: Arc<PlaybackState>,
}

impl Output {
    pub fn open(device: Device, request: &Request, consumer: Consumer<u16>) -> Result<Self> {
        let Request {
            frame_rate,
            channels,
            carrier,
            exclusive,
            buffer_frames,
        } = *request;
        let choice = choose_stream(&device, request)?;
        let (stream_index, stream) = (choice.index, choice.stream);
        let original_format = stream.physical_format()?;
        let original_rate = device.nominal_sample_rate()?;

        let was_default_output = Device::default_output().is_ok_and(|current| current == device);
        let mut hogged = false;
        let mut mixing_disabled = false;
        if exclusive {
            match device.set_hog_mode(unsafe { libc::getpid() }) {
                Ok(()) => hogged = true,
                Err(error) => {
                    if let Some(owner) = device.hog_owner() {
                        // ESRCH means the claim outlived the process that made it; macOS
                        // reclaims those on its own, but not always immediately.
                        let alive = unsafe { libc::kill(owner, 0) } == 0;
                        let advice = if alive {
                            "quit it, or pass --shared to play alongside it"
                        } else {
                            "that process is gone, so the claim should clear shortly; retry, or \
                             unplug and replug the device"
                        };
                        bail!("the device is held exclusively by process {owner}; {advice}");
                    }
                    warn!("could not take exclusive access: {error}");
                }
            }
            if device.supports_mixing_switch() {
                match device.set_mixing(false) {
                    Ok(()) => mixing_disabled = true,
                    Err(error) => debug!("could not disable mixing: {error}"),
                }
            }
        }

        let mut output = Self {
            device,
            stream,
            proc_id: None,
            context: std::ptr::null_mut(),
            original_format,
            original_rate,
            mixing_disabled,
            hogged,
            running: false,
            format_changed: false,
            was_default_output,
            carrier,
            encoding: Encoding::Float32,
            buffer_frames: 0,
            state: Arc::new(PlaybackState::default()),
        };

        if let Err(error) = device.set_nominal_sample_rate(f64::from(frame_rate)) {
            debug!("nominal sample rate not settable directly: {error}");
        }
        let format = adopt_format(&stream, &choice.formats, frame_rate)
            .with_context(|| format!("device rejected every {frame_rate} Hz format"))?;
        output.format_changed = true;
        output.publish_claim();

        let virtual_format = negotiate_virtual_format(&stream, &format, carrier, exclusive)?;
        if virtual_format.mSampleRate != f64::from(frame_rate) {
            bail!(
                "device settled on {} Hz instead of {frame_rate} Hz",
                virtual_format.mSampleRate
            );
        }
        // Native DSD only survives while the mixer is out of the path and the bits go out
        // unconverted. A virtual format that fell back to anything else would send the DAC
        // PCM through its DSD alternate setting, which is loud noise rather than a fault.
        if carrier == Carrier::NativeDsd && !is_native_dsd(&virtual_format) {
            bail!(
                "device would not keep the native DSD format at {frame_rate} Hz; it settled on \
                 {}",
                FormatLine(virtual_format)
            );
        }
        output.encoding = Encoding::from_format(&virtual_format)?;
        if let Some(frames) = buffer_frames {
            device.set_buffer_frame_size(frames)?;
        }
        output.buffer_frames = device.buffer_frame_size().unwrap_or(0);

        let context = Box::into_raw(Box::new(IoContext {
            consumer,
            channels: channels as usize,
            stream_index,
            carrier,
            encoding: output.encoding,
            marker: Marker::new(),
            state: Arc::clone(&output.state),
        }));
        output.context = context;

        output.create_proc()?;
        Ok(output)
    }

    /// Bind the render callback to the device, replacing any previous binding.
    fn create_proc(&mut self) -> Result<()> {
        if self.proc_id.is_some() {
            unsafe { AudioDeviceDestroyIOProcID(self.device.0, self.proc_id) };
            self.proc_id = None;
        }
        let mut proc_id: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                self.device.0,
                Some(io_proc),
                self.context.cast::<c_void>(),
                &mut proc_id,
            )
        };
        if status != 0 {
            bail!(
                "AudioDeviceCreateIOProcID failed: {}",
                hal::status_text(status)
            );
        }
        self.proc_id = proc_id;
        Ok(())
    }

    /// A device that has just changed sample rate rejects the start of a callback bound
    /// before the change, so a refused start rebinds the callback and tries again.
    /// Mirror the claim into the global record the signal path reads.
    fn publish_claim(&self) {
        let reclaim = Reclaim {
            device: self.device,
            stream: self.stream,
            format: self.format_changed.then_some(self.original_format),
            rate: self.original_rate,
            mixing_disabled: self.mixing_disabled,
            hogged: self.hogged,
            was_default_output: self.was_default_output,
        };
        *CLAIMED.lock().unwrap_or_else(|error| error.into_inner()) = Some(reclaim);
    }

    pub fn start(&mut self) -> Result<()> {
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            let status = unsafe { AudioDeviceStart(self.device.0, self.proc_id) };
            if status == 0 {
                self.running = true;
                return Ok(());
            }
            if Instant::now() >= deadline {
                if status == EAGAIN {
                    return Err(DeviceBusy.into());
                }
                bail!("AudioDeviceStart failed: {}", hal::status_text(status));
            }
            debug!(
                "device refused to start ({}); rebinding the callback",
                hal::status_text(status)
            );
            std::thread::sleep(Duration::from_millis(50));
            self.create_proc()?;
        }
    }

    pub fn stop(&mut self) {
        if !self.running {
            return;
        }
        let status = unsafe { AudioDeviceStop(self.device.0, self.proc_id) };
        if status != 0 {
            warn!("AudioDeviceStop failed: {}", hal::status_text(status));
        }
        self.running = false;
    }

    pub fn is_exclusive(&self) -> bool {
        self.hogged
    }

    pub fn mixing_disabled(&self) -> bool {
        self.mixing_disabled
    }

    /// True when the samples reach the device exactly as written. Native DSD always does:
    /// it only exists over a non-mixable format the HAL leaves alone.
    pub fn is_integer(&self) -> bool {
        match self.carrier {
            Carrier::Dop => self.encoding.is_integer(),
            Carrier::NativeDsd => true,
        }
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.stop();
        if self.proc_id.is_some() {
            unsafe { AudioDeviceDestroyIOProcID(self.device.0, self.proc_id) };
        }
        if !self.context.is_null() {
            drop(unsafe { Box::from_raw(self.context) });
        }
        release_claimed_device();
    }
}

/// Everything a claimed device needs undone, in a form a signal handler can act on.
#[derive(Clone, Copy)]
struct Reclaim {
    device: Device,
    stream: Stream,
    format: Option<AudioStreamBasicDescription>,
    rate: f64,
    mixing_disabled: bool,
    hogged: bool,
    was_default_output: bool,
}

/// The device this process currently holds, so a signal can hand it back even though the
/// owning [`Output`] lives on another thread and will never be dropped.
static CLAIMED: Mutex<Option<Reclaim>> = Mutex::new(None);

fn restore(reclaim: &Reclaim) {
    // Wait for the restore to land: leaving a reconfiguration in flight makes the next
    // track's format change fail with EAGAIN.
    if let Some(format) = reclaim.format {
        if let Err(error) = reclaim.stream.set_physical_format(&format) {
            warn!("could not restore the stream format: {error}");
        } else if let Err(error) = settle(&reclaim.stream, format.mSampleRate) {
            debug!("stream format did not settle back: {error}");
        }
    }
    if let Err(error) = reclaim.device.set_nominal_sample_rate(reclaim.rate) {
        debug!("could not restore the sample rate: {error}");
    }
    if reclaim.mixing_disabled
        && let Err(error) = reclaim.device.set_mixing(true)
    {
        warn!("could not re-enable mixing: {error}");
    }
    if reclaim.hogged
        && let Err(error) = reclaim.device.set_hog_mode(-1)
    {
        warn!("could not release exclusive access: {error}");
    }
    // Claiming a device moves the system output elsewhere, and macOS does not move it back.
    let moved_away = reclaim.was_default_output
        && Device::default_output().is_ok_and(|current| current != reclaim.device);
    if moved_away && let Err(error) = reclaim.device.make_default_output() {
        debug!("could not restore the system output device: {error}");
    }
}

/// Hand back whatever device this process holds. Safe to call from the signal thread, and
/// safe to call twice: the second call finds nothing to do.
pub fn release_claimed_device() {
    let claimed = CLAIMED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some(reclaim) = claimed {
        restore(&reclaim);
    }
}

/// The output stream to drive, with the formats it can carry ordered best first.
struct StreamChoice {
    index: usize,
    stream: Stream,
    formats: Vec<AudioStreamBasicDescription>,
    top_score: i32,
}

fn choose_stream(device: &Device, request: &Request) -> Result<StreamChoice> {
    let Request {
        frame_rate,
        channels,
        carrier,
        exclusive,
        ..
    } = *request;
    let mut best: Option<StreamChoice> = None;
    for (index, stream) in device.output_streams()?.iter().enumerate() {
        let mut scored = Vec::new();
        for ranged in stream.available_physical_formats()? {
            let format = ranged.mFormat;
            if !carries_rate(&ranged, frame_rate) || format.mChannelsPerFrame < u32::from(channels)
            {
                continue;
            }
            // The two carriers want disjoint sets of formats, and each is wrong for the
            // other: DoP written into the native format reaches the DAC as raw DSD, and
            // native bits written into a PCM format reach it as a PCM square wave.
            if carrier == Carrier::NativeDsd {
                if is_native_dsd(&format) {
                    scored.push((0, format));
                }
                continue;
            }
            if is_native_dsd(&format) {
                continue;
            }
            let Ok(encoding) = Encoding::from_format(&format) else {
                continue;
            };
            scored.push((
                score(&format, encoding, u32::from(channels), exclusive),
                format,
            ));
        }
        scored.sort_by_key(|(score, _)| Reverse(*score));
        let Some((top_score, _)) = scored.first().copied() else {
            continue;
        };
        if best.as_ref().is_none_or(|best| top_score > best.top_score) {
            best = Some(StreamChoice {
                index,
                stream: *stream,
                formats: scored.into_iter().map(|(_, format)| format).collect(),
                top_score,
            });
        }
    }
    best.with_context(|| match carrier {
        Carrier::Dop => format!(
            "device has no {frame_rate} Hz, {channels} channel, 24 bit or better output format"
        ),
        Carrier::NativeDsd => format!(
            "device publishes no native DSD format at {frame_rate} Hz for {channels} channels"
        ),
    })
}

/// Adopt the best format the device actually accepts, at the requested rate.
fn adopt_format(
    stream: &Stream,
    candidates: &[AudioStreamBasicDescription],
    frame_rate: u32,
) -> Result<AudioStreamBasicDescription> {
    let mut last = None;
    for format in candidates {
        let mut format = *format;
        format.mSampleRate = f64::from(frame_rate);
        match stream
            .set_physical_format(&format)
            .and_then(|()| settle(stream, f64::from(frame_rate)))
        {
            Ok(()) => return Ok(format),
            Err(error) => {
                debug!(
                    "device rejected {} bit format: {error}",
                    format.mBitsPerChannel
                );
                last = Some(error);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("device offers no usable format at {frame_rate} Hz")))
}

/// Changing the hardware format is asynchronous; wait for the device to settle.
fn settle(stream: &Stream, rate: f64) -> Result<()> {
    let deadline = Instant::now() + FORMAT_TIMEOUT;
    loop {
        if stream.physical_format()?.mSampleRate == rate {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("device did not switch to {rate} Hz within {FORMAT_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The callback is handed the stream's virtual format, so what it is must be settled before
/// the callback exists: writing float samples into an integer buffer would be pure noise.
///
/// A non-mixable physical format takes the mixer out of the path, and the virtual format then
/// becomes the physical one - but not instantly, so wait for it rather than sampling too early.
fn negotiate_virtual_format(
    stream: &Stream,
    physical: &AudioStreamBasicDescription,
    carrier: Carrier,
    exclusive: bool,
) -> Result<AudioStreamBasicDescription> {
    if is_non_mixable(physical) {
        let deadline = Instant::now() + VIRTUAL_FORMAT_TIMEOUT;
        loop {
            let current = stream.virtual_format()?;
            // Native DSD needs the virtual format to be the native one, not merely an
            // integer one: the HAL would otherwise hand the callback little-endian PCM
            // samples while the driver streams whatever they are down the DSD endpoint.
            let settled = match carrier {
                Carrier::Dop => Encoding::from_format(&current).is_ok_and(Encoding::is_integer),
                Carrier::NativeDsd => is_native_dsd(&current),
            };
            if settled {
                return Ok(current);
            }
            if Instant::now() >= deadline {
                debug!("device kept a float virtual format under a non-mixable physical one");
                return Ok(current);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let current = stream.virtual_format()?;
    let wants_integer = exclusive
        && Encoding::from_format(physical).is_ok_and(Encoding::is_integer)
        && !Encoding::from_format(&current).is_ok_and(Encoding::is_integer);
    if wants_integer && stream.set_virtual_format(physical).is_ok() {
        return stream.virtual_format();
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use rtrb::RingBuffer;

    use crate::dop::{MARKER_A, MARKER_B, SILENCE_PAYLOAD, pack_planes, split_word};
    use crate::dsd::DSD_SILENCE_BYTE;
    use crate::native;
    use crate::output::encoding::{Carrier, Encoding};
    use crate::output::stream::{IoContext, Marker, PlaybackState};
    use crate::reader::DsdSource;
    use crate::reader::dsf::DsfReader;
    use crate::reader::dsf::tests::{BLOCK, dsf_file};

    const ENCODING: Encoding = Encoding::Signed {
        bytes: 3,
        shift: 0,
        big_endian: false,
    };

    /// Pull a whole file through the reader and the DoP packer.
    fn payloads_of(file: Vec<u8>) -> (Vec<u16>, Vec<Vec<u8>>) {
        let mut source = DsfReader::new(Cursor::new(file)).expect("parses");
        let channels = source.format().channels as usize;
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; source.chunk_bytes()].into(); channels];
        let mut payloads = Vec::new();
        let mut expected = vec![Vec::new(); channels];
        loop {
            let count = source.read(&mut planes).expect("reads");
            if count == 0 {
                return (payloads, expected);
            }
            let slices: Vec<&[u8]> = planes.iter().map(|plane| &plane[..count]).collect();
            for (channel, slice) in slices.iter().enumerate() {
                expected[channel].extend_from_slice(slice);
            }
            pack_planes(&slices, &mut payloads);
        }
    }

    fn context(payloads: &[u16], stream_channels: usize) -> (IoContext, Arc<PlaybackState>) {
        carried(payloads, stream_channels, Carrier::Dop)
    }

    fn carried(
        payloads: &[u16],
        stream_channels: usize,
        carrier: Carrier,
    ) -> (IoContext, Arc<PlaybackState>) {
        let (mut producer, consumer) = RingBuffer::<u16>::new(payloads.len().max(1) * 2);
        for payload in payloads {
            producer.push(*payload).expect("ring has room");
        }
        let state = Arc::new(PlaybackState::default());
        let context = IoContext {
            consumer,
            channels: 2,
            stream_index: 0,
            carrier,
            encoding: ENCODING,
            marker: Marker::new(),
            state: Arc::clone(&state),
        };
        assert!(stream_channels >= 2);
        (context, state)
    }

    /// Decode the device buffer back into per-channel DSD bytes, checking marker alternation.
    fn decode(buffer: &[u8], stream_channels: usize, frame: &mut usize) -> Vec<Vec<u8>> {
        let mut channels = vec![Vec::new(); stream_channels];
        for chunk in buffer.chunks_exact(3 * stream_channels) {
            let expected = if *frame % 2 == 0 { MARKER_A } else { MARKER_B };
            for (channel, sample) in chunk.chunks_exact(3).enumerate() {
                let word = i32::from_le_bytes([sample[0], sample[1], sample[2], 0]) << 8 >> 8;
                let (marker, payload) = split_word(word);
                assert_eq!(marker, expected, "frame {frame} channel {channel}");
                channels[channel].extend_from_slice(&payload.to_be_bytes());
            }
            *frame += 1;
        }
        channels
    }

    #[test]
    fn dsd_reaches_the_device_buffer_bit_for_bit() {
        let (payloads, expected) = payloads_of(dsf_file(1, (BLOCK * 4 * 8) as u64, 4));
        let (mut context, state) = context(&payloads, 2);

        // An odd buffer length makes every render cross ring and block boundaries.
        let mut decoded = vec![Vec::new(); 2];
        let mut frame = 0;
        for _ in 0..12 {
            let mut buffer = vec![0_u8; 3 * 2 * 7];
            context.fill(&mut buffer, 2);
            for (channel, bytes) in decode(&buffer, 2, &mut frame).into_iter().enumerate() {
                decoded[channel].extend_from_slice(&bytes);
            }
        }

        for channel in 0..2 {
            let length = expected[channel].len();
            assert_eq!(decoded[channel][..length], expected[channel][..]);
        }
        assert_eq!(
            state.frames_played.load(Ordering::Relaxed),
            payloads.len() as u64 / 2
        );
    }

    /// Decode a native device buffer back into per-channel DSD bytes. There is no marker to
    /// check: the frame is four raw DSD bytes per channel, earliest byte first.
    fn decode_native(buffer: &[u8], stream_channels: usize) -> Vec<Vec<u8>> {
        let mut channels = vec![Vec::new(); stream_channels];
        let stride = native::BYTES_PER_SUBSLOT;
        for frame in buffer.chunks_exact(stride * stream_channels) {
            for (channel, subslot) in frame.chunks_exact(stride).enumerate() {
                channels[channel].extend_from_slice(subslot);
            }
        }
        channels
    }

    #[test]
    fn native_dsd_reaches_the_device_buffer_bit_for_bit() {
        let (payloads, expected) = payloads_of(dsf_file(1, (BLOCK * 4 * 8) as u64, 4));
        let (mut context, state) = carried(&payloads, 2, Carrier::NativeDsd);

        // An odd buffer length makes every render cross ring and block boundaries.
        let mut decoded = vec![Vec::new(); 2];
        for _ in 0..12 {
            let mut buffer = vec![0_u8; native::frame_bytes(2) * 7];
            context.fill(&mut buffer, 2);
            for (channel, bytes) in decode_native(&buffer, 2).into_iter().enumerate() {
                decoded[channel].extend_from_slice(&bytes);
            }
        }

        for channel in 0..2 {
            let length = expected[channel].len();
            assert_eq!(decoded[channel][..length], expected[channel][..]);
        }
        // Two payloads a channel to a frame, so half the frames DoP would have made.
        assert_eq!(
            state.frames_played.load(Ordering::Relaxed),
            payloads.len() as u64 / 4
        );
    }

    #[test]
    fn a_native_frame_is_what_the_usb_packer_would_have_written() {
        let left: Vec<u8> = (0..=63).collect();
        let right: Vec<u8> = (0..=63).rev().collect();
        let mut payloads = Vec::new();
        pack_planes(&[&left, &right], &mut payloads);
        let (mut context, _) = carried(&payloads, 2, Carrier::NativeDsd);
        let mut expected = Vec::new();
        native::pack_planes(&[&left, &right], &mut expected);

        let mut buffer = vec![0_u8; expected.len()];
        context.fill(&mut buffer, 2);

        assert_eq!(buffer, expected);
    }

    #[test]
    fn an_empty_native_queue_renders_dsd_silence_too() {
        let (mut context, state) = carried(&[], 2, Carrier::NativeDsd);
        let mut buffer = vec![0_u8; native::frame_bytes(2) * 4];

        context.fill(&mut buffer, 2);

        assert_eq!(buffer, vec![DSD_SILENCE_BYTE; native::frame_bytes(2) * 4]);
        assert_eq!(state.underrun_frames.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn a_native_channel_beyond_the_file_gets_dsd_silence() {
        let left: Vec<u8> = (1..=8).collect();
        let right: Vec<u8> = (9..=16).collect();
        let mut payloads = Vec::new();
        pack_planes(&[&left, &right], &mut payloads);
        let (mut context, _) = carried(&payloads, 4, Carrier::NativeDsd);

        let mut buffer = vec![0_u8; native::frame_bytes(4) * 2];
        context.fill(&mut buffer, 4);

        let decoded = decode_native(&buffer, 4);
        assert_eq!(decoded[0], left);
        assert_eq!(decoded[1], right);
        assert_eq!(decoded[2], vec![DSD_SILENCE_BYTE; 8]);
        assert_eq!(decoded[3], vec![DSD_SILENCE_BYTE; 8]);
    }

    #[test]
    fn an_empty_queue_renders_dsd_silence_instead_of_pcm_zero() {
        let (mut context, state) = context(&[], 2);
        let mut buffer = vec![0_u8; 3 * 2 * 4];

        context.fill(&mut buffer, 2);

        let mut frame = 0;
        for channel in decode(&buffer, 2, &mut frame) {
            assert_eq!(channel, vec![0x69; 8]);
        }
        assert_eq!(state.underrun_frames.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn channels_beyond_the_file_get_dsd_silence() {
        let (payloads, _) = payloads_of(dsf_file(8, (BLOCK * 8) as u64, 1));
        let (mut context, _) = context(&payloads, 4);
        let mut buffer = vec![0_u8; 3 * 4 * 4];

        context.fill(&mut buffer, 4);

        let mut frame = 0;
        let decoded = decode(&buffer, 4, &mut frame);
        assert_eq!(decoded[2], SILENCE_PAYLOAD.to_be_bytes().repeat(4));
        assert_eq!(decoded[3], decoded[2]);
    }

    #[test]
    fn pausing_holds_the_queue_and_keeps_the_dac_fed_with_dsd_silence() {
        let (payloads, _) = payloads_of(dsf_file(8, (BLOCK * 8) as u64, 1));
        let queued = payloads.len();
        let (mut context, state) = context(&payloads, 2);
        state.paused.store(true, Ordering::Relaxed);
        let mut buffer = vec![0_u8; 3 * 2 * 4];

        context.fill(&mut buffer, 2);

        let mut frame = 0;
        assert_eq!(decode(&buffer, 2, &mut frame)[0], vec![0x69; 8]);
        assert_eq!(state.frames_played.load(Ordering::Relaxed), 0);
        // A pause is not a dropout, and nothing is consumed, so resuming loses no audio.
        assert_eq!(state.underrun_frames.load(Ordering::Relaxed), 0);
        assert_eq!(context.consumer.slots(), queued);

        state.paused.store(false, Ordering::Relaxed);
        context.fill(&mut buffer, 2);
        assert_eq!(state.frames_played.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn the_silence_flag_stops_the_queue_from_draining() {
        let (payloads, _) = payloads_of(dsf_file(8, (BLOCK * 8) as u64, 1));
        let (mut context, state) = context(&payloads, 2);
        state.silence.store(true, Ordering::Relaxed);
        let mut buffer = vec![0_u8; 3 * 2 * 4];

        context.fill(&mut buffer, 2);

        let mut frame = 0;
        assert_eq!(decode(&buffer, 2, &mut frame)[0], vec![0x69; 8]);
        assert_eq!(state.frames_played.load(Ordering::Relaxed), 0);
        assert_eq!(state.underrun_frames.load(Ordering::Relaxed), 0);
    }
}
