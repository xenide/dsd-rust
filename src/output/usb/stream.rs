//! The isochronous engine that feeds native DSD to a held DAC.
//!
//! Everything here runs on one thread that owns a CFRunLoop: IOKit delivers transfer
//! completions to it, and each completion refills its slot and resubmits. Sizing comes from
//! the DAC's own feedback endpoint, because an asynchronous endpoint runs on the DAC's clock
//! and not the host's -- open loop drifts.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, bail};
use rtrb::Consumer;
use tracing::debug;

use crate::dsd::DSD_SILENCE_BYTE;
use crate::output::stream::PlaybackState;
use crate::output::usb::device::Held;
use crate::output::usb::sys;

/// Microframes per transfer: 4 ms of audio.
const UFRAMES_PER_XFER: usize = 32;
/// Transfers in flight, so 32 ms of scheduling headroom.
const NUM_SLOTS: usize = 8;
/// High-speed USB divides each 1 ms bus frame into 8 microframes.
const UFRAMES_PER_MS: u64 = 8;
/// Schedule the first transfer this many bus frames ahead of now.
const START_LEAD: u64 = 10;

/// Feedback outside this band is a decoding error, not a real clock, and is ignored.
const MIN_RATIO: f64 = 0.95;
const MAX_RATIO: f64 = 1.05;

/// Retries before a chain is declared dead rather than merely late.
const SUBMIT_ATTEMPTS: usize = 3;

macro_rules! call {
    ($iface:expr, $method:ident $(, $arg:expr)* $(,)?) => {{
        let method = (**$iface)
            .$method
            .expect(concat!("IOKit vtable provides ", stringify!($method)));
        method($iface.cast::<c_void>() $(, $arg)*)
    }};
}

fn succeeded(result: sys::IOReturn) -> bool {
    result == sys::kIOReturnSuccess as sys::IOReturn
}

/// A transfer is late rather than broken when the bus has already passed its frame.
fn tolerable(result: sys::IOReturn) -> bool {
    succeeded(result) || result == sys::kIOReturnUnderrun as sys::IOReturn
}

/// One transfer's data buffer and frame list, both allocated by IOKit for DMA.
struct Slot {
    data: *mut c_void,
    frames: *mut sys::IOUSBLowLatencyIsocFrame,
}

/// Identifies which slot a completion belongs to. IOKit hands this back as the refcon.
struct Token {
    engine: *mut Engine,
    slot: usize,
}

struct Engine {
    held: Held,
    out_pipe: u8,
    feedback_pipe: Option<u8>,
    slots: Vec<Slot>,
    feedback: Option<Slot>,
    next_frame: u64,
    feedback_frame: u64,
    /// Samples per microframe the DAC is currently asking for.
    samples: f64,
    nominal: f64,
    /// Fractional carry, so a non-integer rate averages out exactly.
    carry: f64,
    frame_bytes: usize,
    consumer: Consumer<u8>,
    state: Arc<PlaybackState>,
    stop: Arc<AtomicBool>,
    running: bool,
    feedback_updates: u64,
}

impl Engine {
    /// Copy queued DSD into one microframe, padding with DSD silence rather than zero so an
    /// underrun never drops the DAC out of lock.
    fn fill(&mut self, dst: &mut [u8]) {
        let held =
            self.state.silence.load(Ordering::Relaxed) || self.state.paused.load(Ordering::Relaxed);
        if held {
            dst.fill(DSD_SILENCE_BYTE);
            return;
        }

        let want = dst.len();
        let available = self.consumer.slots().min(want);
        let mut written = 0;
        if available > 0
            && let Ok(chunk) = self.consumer.read_chunk(available)
        {
            let (head, tail) = chunk.as_slices();
            dst[..head.len()].copy_from_slice(head);
            dst[head.len()..head.len() + tail.len()].copy_from_slice(tail);
            written = head.len() + tail.len();
            chunk.commit_all();
        }
        if written < want {
            dst[written..].fill(DSD_SILENCE_BYTE);
            let short = (want - written) / self.frame_bytes;
            self.state
                .underrun_frames
                .fetch_add(short as u64, Ordering::Relaxed);
        }
        self.state
            .frames_played
            .fetch_add((written / self.frame_bytes) as u64, Ordering::Relaxed);
    }

    /// Size every microframe of one transfer and hand it to IOKit.
    fn submit(&mut self, slot: usize) {
        let data = self.slots[slot].data.cast::<u8>();
        let frames = self.slots[slot].frames;
        let mut offset = 0;

        for index in 0..UFRAMES_PER_XFER {
            self.carry += self.samples;
            let count = self.carry as usize;
            self.carry -= count as f64;
            let bytes = count * self.frame_bytes;

            // SAFETY: `data` is an IOKit buffer of UFRAMES_PER_XFER * max_packet bytes, and
            // `bytes` never exceeds max_packet for a rate the endpoint advertised.
            let slice = unsafe { std::slice::from_raw_parts_mut(data.add(offset), bytes) };
            self.fill(slice);

            // SAFETY: `frames` has UFRAMES_PER_XFER entries.
            unsafe {
                let frame = frames.add(index);
                (*frame).frStatus = -1;
                (*frame).frReqCount = u16::try_from(bytes).unwrap_or(u16::MAX);
                (*frame).frActCount = 0;
            }
            offset += bytes;
        }

        let token = Box::into_raw(Box::new(Token {
            engine: ptr::from_mut(self),
            slot,
        }));
        for _ in 0..SUBMIT_ATTEMPTS {
            // SAFETY: the interface is held open and the buffers outlive the transfer.
            let result = unsafe {
                call!(
                    self.held.streaming(),
                    LowLatencyWriteIsochPipeAsync,
                    self.out_pipe,
                    self.slots[slot].data,
                    self.next_frame,
                    UFRAMES_PER_XFER as u32,
                    0,
                    self.slots[slot].frames,
                    Some(on_complete),
                    token.cast::<c_void>(),
                )
            };
            if succeeded(result) {
                self.next_frame += UFRAMES_PER_XFER as u64 / UFRAMES_PER_MS;
                return;
            }
            self.resync_output();
        }
        // SAFETY: no callback will fire for a transfer that never started.
        drop(unsafe { Box::from_raw(token) });
        debug!("native DSD output chain stopped: no transfer would schedule");
        self.running = false;
    }

    fn on_complete(&mut self, slot: usize, result: sys::IOReturn) {
        if !tolerable(result) {
            debug!("native DSD transfer failed: IOReturn 0x{result:08x}");
            self.running = false;
            return;
        }
        if self.running && !self.stop.load(Ordering::Relaxed) {
            self.submit(slot);
        }
    }

    /// Read the DAC's requested rate. High-speed feedback is 16.16 samples per microframe.
    fn on_feedback(&mut self, result: sys::IOReturn) {
        if tolerable(result)
            && let Some(feedback) = self.feedback.as_ref()
        {
            for index in 0..UFRAMES_PER_XFER {
                // SAFETY: the frame list and its buffer both have UFRAMES_PER_XFER entries.
                let (actual, raw) = unsafe {
                    let frame = feedback.frames.add(index);
                    let bytes = feedback.data.cast::<u8>().add(index * 4);
                    (
                        (*frame).frActCount,
                        u32::from_le_bytes([*bytes, *bytes.add(1), *bytes.add(2), *bytes.add(3)]),
                    )
                };
                if actual < 4 {
                    continue;
                }
                let samples = f64::from(raw) / 65_536.0;
                let ratio = samples / self.nominal;
                if (MIN_RATIO..=MAX_RATIO).contains(&ratio) {
                    self.samples = samples;
                    self.feedback_updates += 1;
                }
            }
        }
        // Always resubmit: a servo that stops silently stops tracking the DAC's clock.
        if self.running && !self.stop.load(Ordering::Relaxed) {
            self.submit_feedback();
        }
    }

    fn submit_feedback(&mut self) {
        let Some(feedback) = self.feedback.as_ref() else {
            return;
        };
        let Some(pipe) = self.feedback_pipe else {
            return;
        };
        let (data, frames) = (feedback.data, feedback.frames);

        for _ in 0..SUBMIT_ATTEMPTS {
            for index in 0..UFRAMES_PER_XFER {
                // SAFETY: `frames` has UFRAMES_PER_XFER entries.
                unsafe {
                    let frame = frames.add(index);
                    (*frame).frStatus = -1;
                    (*frame).frReqCount = 4;
                    (*frame).frActCount = 0;
                }
            }
            let token = Box::into_raw(Box::new(Token {
                engine: ptr::from_mut(self),
                slot: 0,
            }));
            // SAFETY: as for the output chain.
            let result = unsafe {
                call!(
                    self.held.streaming(),
                    LowLatencyReadIsochPipeAsync,
                    pipe,
                    data,
                    self.feedback_frame,
                    UFRAMES_PER_XFER as u32,
                    0,
                    frames,
                    Some(on_feedback),
                    token.cast::<c_void>(),
                )
            };
            if succeeded(result) {
                self.feedback_frame += UFRAMES_PER_XFER as u64 / UFRAMES_PER_MS;
                return;
            }
            // SAFETY: no callback fires for a transfer that never started.
            drop(unsafe { Box::from_raw(token) });
            self.feedback_frame = 0;
            self.resync_feedback();
        }
        debug!("native DSD feedback chain stopped; holding the last rate");
        self.feedback_pipe = None;
    }

    fn bus_frame(&self) -> Option<u64> {
        let mut frame = 0_u64;
        let mut at = sys::AbsoluteTime::default();
        // SAFETY: the interface is open.
        let result = unsafe {
            call!(
                self.held.streaming(),
                GetBusFrameNumber,
                &raw mut frame,
                &raw mut at
            )
        };
        succeeded(result).then_some(frame)
    }

    /// Re-anchor a chain whose next frame has already passed on the bus.
    fn resync_output(&mut self) {
        if let Some(now) = self.bus_frame()
            && self.next_frame <= now + 2
        {
            self.next_frame = now + START_LEAD / 2;
        }
    }

    fn resync_feedback(&mut self) {
        if let Some(now) = self.bus_frame()
            && self.feedback_frame <= now + 2
        {
            self.feedback_frame = now + START_LEAD / 2;
        }
    }
}

/// IOKit calls this on the engine thread's run loop when an output transfer completes.
unsafe extern "C" fn on_complete(refcon: *mut c_void, result: sys::IOReturn, _arg: *mut c_void) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: the token was leaked by `submit` and is reclaimed exactly once, here.
    let token = unsafe { Box::from_raw(refcon.cast::<Token>()) };
    // SAFETY: the engine outlives every transfer, because the thread joins them first.
    unsafe { (*token.engine).on_complete(token.slot, result) };
}

unsafe extern "C" fn on_feedback(refcon: *mut c_void, result: sys::IOReturn, _arg: *mut c_void) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: as above.
    let token = unsafe { Box::from_raw(refcon.cast::<Token>()) };
    // SAFETY: as above.
    unsafe { (*token.engine).on_feedback(result) };
}

/// A running native DSD stream. Dropping it stops the engine and hands the DAC back.
pub struct NativeStream {
    stop: Arc<AtomicBool>,
    engine: Option<JoinHandle<()>>,
}

impl NativeStream {
    /// Configure the DAC, allocate the transfer buffers, and start streaming.
    pub fn start(
        held: Held,
        consumer: Consumer<u8>,
        channels: usize,
        dsd_rate: u32,
        state: Arc<PlaybackState>,
    ) -> Result<Self> {
        let frame_rate = held.configure(dsd_rate)?;
        let (out_pipe, feedback_pipe) = held.pipes()?;
        let name = held.name.clone();
        let frame_bytes = channels * crate::native::BYTES_PER_SUBSLOT;
        let max_packet = held.native.max_packet as usize;
        let nominal = f64::from(frame_rate) / (UFRAMES_PER_MS as f64 * 1000.0);
        if nominal * frame_bytes as f64 > max_packet as f64 {
            bail!(
                "{name}: {dsd_rate} needs {:.0} bytes per microframe but the endpoint takes \
                 only {max_packet}",
                nominal * frame_bytes as f64
            );
        }

        let stop = Arc::new(AtomicBool::new(false));
        let thread_state = Arc::clone(&state);
        let thread_stop = Arc::clone(&stop);
        let engine = thread::Builder::new()
            .name("dsd-usb".to_owned())
            .spawn(move || {
                run(
                    held,
                    consumer,
                    thread_state,
                    thread_stop,
                    out_pipe,
                    feedback_pipe,
                    frame_bytes,
                    nominal,
                    max_packet,
                );
            })?;

        Ok(Self {
            stop,
            engine: Some(engine),
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(engine) = self.engine.take() {
            let _ = engine.join();
        }
    }
}

impl Drop for NativeStream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The engine thread: owns the run loop, the buffers, and the DAC.
#[allow(clippy::too_many_arguments)]
fn run(
    held: Held,
    consumer: Consumer<u8>,
    state: Arc<PlaybackState>,
    stop: Arc<AtomicBool>,
    out_pipe: u8,
    feedback_pipe: Option<u8>,
    frame_bytes: usize,
    nominal: f64,
    max_packet: usize,
) {
    let streaming = held.streaming();
    let mut engine = Box::new(Engine {
        held,
        out_pipe,
        feedback_pipe,
        slots: Vec::new(),
        feedback: None,
        next_frame: 0,
        feedback_frame: 0,
        samples: nominal,
        nominal,
        carry: 0.0,
        frame_bytes,
        consumer,
        state,
        stop: Arc::clone(&stop),
        running: true,
        feedback_updates: 0,
    });

    // SAFETY: every allocation below is destroyed before this function returns.
    unsafe {
        for _ in 0..NUM_SLOTS {
            let Some(slot) = create_slot(streaming, UFRAMES_PER_XFER * max_packet, false) else {
                debug!("native DSD: cannot allocate transfer buffers");
                destroy_slots(streaming, &mut engine);
                return;
            };
            engine.slots.push(slot);
        }
        if feedback_pipe.is_some() {
            engine.feedback = create_slot(streaming, UFRAMES_PER_XFER * 4, true);
            if engine.feedback.is_none() {
                engine.feedback_pipe = None;
            }
        }

        let mut source: sys::CFRunLoopSourceRef = ptr::null_mut();
        if !succeeded(call!(
            streaming,
            CreateInterfaceAsyncEventSource,
            &raw mut source
        )) {
            debug!("native DSD: cannot create an async event source");
            destroy_slots(streaming, &mut engine);
            return;
        }
        sys::CFRunLoopAddSource(
            sys::CFRunLoopGetCurrent(),
            source,
            sys::kCFRunLoopDefaultMode,
        );

        let start = engine.bus_frame().unwrap_or(0) + START_LEAD;
        engine.next_frame = start;
        engine.feedback_frame = start;

        if engine.feedback_pipe.is_some() {
            engine.submit_feedback();
        }
        for slot in 0..NUM_SLOTS {
            engine.submit(slot);
        }

        while engine.running && !stop.load(Ordering::Relaxed) {
            sys::CFRunLoopRunInMode(sys::kCFRunLoopDefaultMode, 0.05, 1);
        }
        engine.running = false;
        // Let in-flight completions land so no callback fires after the buffers are gone.
        sys::CFRunLoopRunInMode(sys::kCFRunLoopDefaultMode, 0.25, 1);

        sys::CFRunLoopRemoveSource(
            sys::CFRunLoopGetCurrent(),
            source,
            sys::kCFRunLoopDefaultMode,
        );
        debug!(
            "native DSD engine stopped after {} feedback updates",
            engine.feedback_updates
        );
        engine.held.reset();
        destroy_slots(streaming, &mut engine);
    }
    thread::sleep(Duration::from_millis(1));
}

unsafe fn create_slot(
    streaming: *mut *mut sys::IOUSBInterfaceInterface500,
    bytes: usize,
    read: bool,
) -> Option<Slot> {
    let kind = if read {
        sys::USBLowLatencyBufferType_kUSBLowLatencyReadBuffer
    } else {
        sys::USBLowLatencyBufferType_kUSBLowLatencyWriteBuffer
    };
    let mut data: *mut c_void = ptr::null_mut();
    let mut frames: *mut c_void = ptr::null_mut();
    // SAFETY: the caller destroys both buffers.
    unsafe {
        if !succeeded(call!(
            streaming,
            LowLatencyCreateBuffer,
            &raw mut data,
            bytes as sys::IOByteCount,
            kind
        )) {
            return None;
        }
        let frame_bytes = UFRAMES_PER_XFER * size_of::<sys::IOUSBLowLatencyIsocFrame>();
        if !succeeded(call!(
            streaming,
            LowLatencyCreateBuffer,
            &raw mut frames,
            frame_bytes as sys::IOByteCount,
            sys::USBLowLatencyBufferType_kUSBLowLatencyFrameListBuffer
        )) {
            call!(streaming, LowLatencyDestroyBuffer, data);
            return None;
        }
        Some(Slot {
            data,
            frames: frames.cast::<sys::IOUSBLowLatencyIsocFrame>(),
        })
    }
}

unsafe fn destroy_slots(streaming: *mut *mut sys::IOUSBInterfaceInterface500, engine: &mut Engine) {
    // SAFETY: no transfer is in flight by the time this runs.
    unsafe {
        for slot in engine.slots.drain(..).chain(engine.feedback.take()) {
            call!(streaming, LowLatencyDestroyBuffer, slot.data);
            call!(
                streaming,
                LowLatencyDestroyBuffer,
                slot.frames.cast::<c_void>()
            );
        }
    }
}
