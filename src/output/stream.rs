use std::os::raw::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use coreaudio_sys::{
    AudioBufferList, AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioObjectID, AudioStreamBasicDescription,
    AudioStreamRangedDescription, AudioTimeStamp, OSStatus, kAudioFormatLinearPCM,
};
use rtrb::Consumer;
use tracing::{debug, warn};

use crate::dop::{self, Marker, SILENCE_PAYLOAD};
use crate::output::encoding::Encoding;
use crate::output::hal::{self, Device, Stream};

/// Counters the render callback publishes for the transport display.
#[derive(Debug, Default)]
pub struct PlaybackState {
    pub frames_played: AtomicU64,
    pub underrun_frames: AtomicU64,
    /// Set to drop whatever is queued and send DoP silence, so playback can end without a pop.
    pub silence: AtomicBool,
}

struct IoContext {
    consumer: Consumer<u16>,
    channels: usize,
    stream_index: usize,
    encoding: Encoding,
    marker: Marker,
    state: Arc<PlaybackState>,
}

impl IoContext {
    fn fill(&mut self, out: &mut [u8], stream_channels: usize) {
        let Self {
            consumer,
            channels,
            encoding,
            marker,
            state,
            ..
        } = self;
        let sample_bytes = encoding.bytes_per_sample();
        let frame_bytes = sample_bytes * stream_channels;
        if frame_bytes == 0 || stream_channels == 0 {
            return;
        }
        let frames = out.len() / frame_bytes;
        let ready = if state.silence.load(Ordering::Relaxed) {
            0
        } else {
            (consumer.slots() / *channels).min(frames)
        };
        let chunk = consumer
            .read_chunk(ready * *channels)
            .expect("counted slots are available");
        let (head, tail) = chunk.as_slices();
        let mut payloads = head.iter().chain(tail).copied();

        let mut cursor = 0;
        for _ in 0..frames {
            let marker = marker.next();
            for channel in 0..stream_channels {
                let payload = if channel < *channels {
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
        chunk.commit_all();

        state
            .frames_played
            .fetch_add(ready as u64, Ordering::Relaxed);
        state
            .underrun_frames
            .fetch_add((frames - ready) as u64, Ordering::Relaxed);
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

/// Prefer integer transport, then 24-bit containers, then an exact channel match.
fn score(format: &AudioStreamBasicDescription, encoding: Encoding, channels: u32) -> i32 {
    let mut score = 0;
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

/// What the player asks of a device.
pub struct Request {
    pub pcm_rate: u32,
    pub channels: u16,
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
    pub encoding: Encoding,
    pub buffer_frames: u32,
    pub state: Arc<PlaybackState>,
}

impl Output {
    pub fn open(device: Device, request: &Request, consumer: Consumer<u16>) -> Result<Self> {
        let Request {
            pcm_rate,
            channels,
            exclusive,
            buffer_frames,
        } = *request;
        let (stream_index, stream, mut format) = choose_format(&device, pcm_rate, channels)?;
        let original_format = stream.physical_format()?;
        let original_rate = device.nominal_sample_rate()?;

        let mut hogged = false;
        let mut mixing_disabled = false;
        if exclusive {
            match device.set_hog_mode(unsafe { libc::getpid() }) {
                Ok(()) => hogged = true,
                Err(error) => warn!("could not take exclusive access: {error}"),
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
            encoding: Encoding::Float32,
            buffer_frames: 0,
            state: Arc::new(PlaybackState::default()),
        };

        format.mSampleRate = f64::from(pcm_rate);
        if let Err(error) = device.set_nominal_sample_rate(format.mSampleRate) {
            debug!("nominal sample rate not settable directly: {error}");
        }
        stream.set_physical_format(&format).with_context(|| {
            format!(
                "device rejected {pcm_rate} Hz {} bit",
                format.mBitsPerChannel
            )
        })?;
        wait_for_format(&stream, pcm_rate)?;

        let virtual_format = negotiate_virtual_format(&stream, &format, exclusive)?;
        if virtual_format.mSampleRate != f64::from(pcm_rate) {
            bail!(
                "device settled on {} Hz instead of {pcm_rate} Hz",
                virtual_format.mSampleRate
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
            encoding: output.encoding,
            marker: Marker::new(),
            state: Arc::clone(&output.state),
        }));
        output.context = context;

        let mut proc_id: AudioDeviceIOProcID = None;
        let status = unsafe {
            AudioDeviceCreateIOProcID(
                device.0,
                Some(io_proc),
                context.cast::<c_void>(),
                &mut proc_id,
            )
        };
        if status != 0 {
            bail!(
                "AudioDeviceCreateIOProcID failed: {}",
                hal::status_text(status)
            );
        }
        output.proc_id = proc_id;
        Ok(output)
    }

    pub fn start(&mut self) -> Result<()> {
        let status = unsafe { AudioDeviceStart(self.device.0, self.proc_id) };
        if status != 0 {
            bail!("AudioDeviceStart failed: {}", hal::status_text(status));
        }
        self.running = true;
        Ok(())
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
        if let Err(error) = self.stream.set_physical_format(&self.original_format) {
            warn!("could not restore the stream format: {error}");
        }
        if let Err(error) = self.device.set_nominal_sample_rate(self.original_rate) {
            debug!("could not restore the sample rate: {error}");
        }
        if self.mixing_disabled
            && let Err(error) = self.device.set_mixing(true)
        {
            warn!("could not re-enable mixing: {error}");
        }
        if self.hogged
            && let Err(error) = self.device.set_hog_mode(-1)
        {
            warn!("could not release exclusive access: {error}");
        }
    }
}

fn choose_format(
    device: &Device,
    pcm_rate: u32,
    channels: u16,
) -> Result<(usize, Stream, AudioStreamBasicDescription)> {
    let streams = device.output_streams()?;
    let mut best: Option<(usize, Stream, AudioStreamBasicDescription, i32)> = None;
    for (index, stream) in streams.iter().enumerate() {
        for ranged in stream.available_physical_formats()? {
            let format = ranged.mFormat;
            if !carries_rate(&ranged, pcm_rate) || format.mChannelsPerFrame < u32::from(channels) {
                continue;
            }
            let Ok(encoding) = Encoding::from_format(&format) else {
                continue;
            };
            let candidate = score(&format, encoding, u32::from(channels));
            if best.as_ref().is_none_or(|(.., best)| candidate > *best) {
                best = Some((index, *stream, format, candidate));
            }
        }
    }
    let (index, stream, format, _) = best.with_context(|| {
        format!("device has no {pcm_rate} Hz, {channels} channel, 24 bit or better output format")
    })?;
    Ok((index, stream, format))
}

/// Changing the hardware format is asynchronous; wait for the device to settle.
fn wait_for_format(stream: &Stream, pcm_rate: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if stream.physical_format()?.mSampleRate == f64::from(pcm_rate) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("device did not switch to {pcm_rate} Hz within 3 s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// In exclusive mode, ask for integer buffers so no float conversion sits in the path.
fn negotiate_virtual_format(
    stream: &Stream,
    physical: &AudioStreamBasicDescription,
    exclusive: bool,
) -> Result<AudioStreamBasicDescription> {
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
    use crate::output::encoding::Encoding;
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
        let (mut producer, consumer) = RingBuffer::<u16>::new(payloads.len().max(1) * 2);
        for payload in payloads {
            producer.push(*payload).expect("ring has room");
        }
        let state = Arc::new(PlaybackState::default());
        let context = IoContext {
            consumer,
            channels: 2,
            stream_index: 0,
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
    fn the_silence_flag_stops_the_queue_from_draining() {
        let (payloads, _) = payloads_of(dsf_file(8, (BLOCK * 8) as u64, 1));
        let (mut context, state) = context(&payloads, 2);
        state.silence.store(true, Ordering::Relaxed);
        let mut buffer = vec![0_u8; 3 * 2 * 4];

        context.fill(&mut buffer, 2);

        let mut frame = 0;
        assert_eq!(decode(&buffer, 2, &mut frame)[0], vec![0x69; 8]);
        assert_eq!(state.frames_played.load(Ordering::Relaxed), 0);
    }
}
