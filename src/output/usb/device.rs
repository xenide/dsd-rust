//! Taking a USB DAC away from `usbaudiod` and configuring its native DSD path.
//!
//! macOS runs USB audio in a userspace daemon, not a kernel driver, so the audio interfaces
//! are held by another process rather than owned by the kernel. Nothing can seize them while
//! that daemon holds them, but the daemon has to re-acquire them after the device
//! re-enumerates, and that leaves a window of roughly 20 ms. This module forces a
//! re-enumeration, wins that race, and then simply never lets go: the interfaces stay open
//! for as long as anything holds the `Held`, which keeps the daemon out.
//!
//! Winning is not guaranteed on the first go. Handing a DAC back re-enumerates it too, so a
//! claim that follows one closely finds its own request swallowed by a re-enumeration
//! already in flight and races a window that has been and gone. `acquire` therefore forces
//! several re-enumerations rather than polling one that will not come.

use std::ffi::{CString, c_void};
use std::ptr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing::debug;

use crate::output::usb::descriptors::{NativeDsd, find_native_dsd};
use crate::output::usb::sys;

type DeviceRef = *mut *mut sys::IOUSBDeviceInterface500;
type InterfaceRef = *mut *mut sys::IOUSBInterfaceInterface500;

/// How long one re-enumeration's window is worth waiting for. The DAC is back on the bus
/// within a few hundred milliseconds, so a race still empty after this missed the window or
/// never had one, and the answer is another re-enumeration rather than more polling.
const RACE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Re-enumerations to force before giving up on the DAC.
const RACE_ATTEMPTS: u32 = 4;
/// How long to wait for the DAC to be on the bus at all before asking it to re-enumerate.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(3);
const RACE_POLL: Duration = Duration::from_micros(500);

/// UAC2 class request: set the current value of a control.
const UAC2_CUR: u8 = 0x01;
/// UAC2 class request: report the valid range of a control.
const UAC2_RANGE: u8 = 0x02;
/// A clock range report is a count followed by (min, max, resolution) triples.
const SUBRANGE_BYTES: usize = 12;
const CS_SAM_FREQ_CONTROL: u16 = 0x01;
/// Host to device, class request, addressed to an interface.
const REQUEST_TYPE_SET: u8 = 0x21;
/// Device to host, class request, addressed to an interface.
const REQUEST_TYPE_GET: u8 = 0xA1;

/// Invoke a method on a COM-style IOKit interface.
macro_rules! call {
    ($iface:expr, $method:ident $(, $arg:expr)* $(,)?) => {{
        let method = (**$iface)
            .$method
            .expect(concat!("IOKit vtable provides ", stringify!($method)));
        method($iface.cast::<c_void>() $(, $arg)*)
    }};
}

/// Whether a USB product name and some other name refer to the same device.
///
/// Core Audio and USB report different names for one device -- "Cayin RU7 Playback" against
/// "Cayin RU7" -- and either may be the longer of the two, so whichever contains the other
/// counts. Matching only one way rejects the very name the `devices` command prints.
fn names_match(usb: &str, query: &str) -> bool {
    let usb = usb.trim().to_lowercase();
    let query = query.trim().to_lowercase();
    !usb.is_empty() && !query.is_empty() && (query.contains(&usb) || usb.contains(&query))
}

fn ok(result: sys::IOReturn, what: &str) -> Result<()> {
    if result == sys::kIOReturnSuccess as sys::IOReturn {
        return Ok(());
    }
    bail!("{what} failed: IOReturn 0x{result:08x}")
}

/// A USB DAC that advertises a native DSD alternate setting.
pub struct Dac {
    service: sys::io_service_t,
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    pub native: NativeDsd,
}

impl Dac {
    /// Every attached USB device whose descriptors expose a native DSD path.
    pub fn discover() -> Result<Vec<Self>> {
        let mut found = Vec::new();
        // SAFETY: the matching dictionary is consumed by IOServiceGetMatchingServices.
        unsafe {
            let class = CString::new("IOUSBHostDevice").expect("literal has no interior nul");
            let matching = sys::IOServiceMatching(class.as_ptr());
            if matching.is_null() {
                bail!("cannot build an IOUSBHostDevice matching dictionary");
            }
            let mut iterator: sys::io_iterator_t = 0;
            ok(
                sys::IOServiceGetMatchingServices(0, matching, &raw mut iterator),
                "IOServiceGetMatchingServices",
            )?;

            loop {
                let service = sys::IOIteratorNext(iterator);
                if service == 0 {
                    break;
                }
                match Self::inspect(service) {
                    Some(dac) => found.push(dac),
                    None => {
                        sys::IOObjectRelease(service);
                    }
                }
            }
            sys::IOObjectRelease(iterator);
        }
        Ok(found)
    }

    /// Read one device's descriptors, keeping it only if it can do native DSD.
    fn inspect(service: sys::io_service_t) -> Option<Self> {
        // SAFETY: `service` is a live IOKit object owned by the caller.
        unsafe {
            let device = device_interface(service)?;
            let result = (|| {
                let config = config_descriptor(device)?;
                let native = find_native_dsd(config).ok()?;
                let mut vendor = 0;
                let mut product = 0;
                call!(device, GetDeviceVendor, &raw mut vendor);
                call!(device, GetDeviceProduct, &raw mut product);
                Some(Self {
                    service,
                    name: registry_string(service, "USB Product Name")
                        .unwrap_or_else(|| format!("USB {vendor:04x}:{product:04x}")),
                    vendor,
                    product,
                    native,
                })
            })();
            call!(device, Release);
            result
        }
    }

    /// Frame rates the clock will actually accept, straight from the DAC's RANGE report.
    ///
    /// The endpoint's packet size only bounds what the wire can carry; it says nothing about
    /// what the converter supports. Reporting the bandwidth ceiling as a capability would
    /// promise rates the DAC rejects.
    pub fn clock_rates(&self) -> Result<Vec<u32>> {
        // SAFETY: the device interface is opened and closed on every path.
        unsafe {
            let Some(device) = device_interface(self.service) else {
                bail!("{}: cannot open a USB device interface", self.name);
            };
            // Plain open only, never a seize: reading the clock range is what `devices`
            // does, and taking a device away from whoever is streaming to it would
            // interrupt their playback just to print a line.
            let opened = call!(device, USBDeviceOpen);
            if opened != sys::kIOReturnSuccess as sys::IOReturn {
                call!(device, Release);
                bail!(
                    "{}: in use by another process, so its clock range cannot be read",
                    self.name
                );
            }

            let mut buffer = [0_u8; 2 + SUBRANGE_BYTES * 32];
            let mut request = sys::IOUSBDevRequest {
                bmRequestType: REQUEST_TYPE_GET,
                bRequest: UAC2_RANGE,
                wValue: CS_SAM_FREQ_CONTROL << 8,
                wIndex: (u16::from(self.native.clock_id) << 8)
                    | u16::from(self.native.control_interface),
                wLength: buffer.len() as u16,
                pData: buffer.as_mut_ptr().cast::<c_void>(),
                wLenDone: 0,
            };
            let result = call!(device, DeviceRequest, &raw mut request);
            call!(device, USBDeviceClose);
            call!(device, Release);
            ok(result, "read clock range")?;
            Ok(parse_clock_ranges(&buffer[..request.wLenDone as usize]))
        }
    }

    /// True when `query` names this DAC.
    pub fn matches(&self, query: &str) -> bool {
        names_match(&self.name, query)
    }

    /// Force a re-enumeration, win the race for both audio interfaces, and hold them.
    ///
    /// A re-enumeration already in flight -- because this or another process has just handed
    /// the DAC back -- swallows the request, and `usbaudiod` then takes the interfaces during
    /// a window this process never saw. Polling harder does not help, because nothing will
    /// open that window again, so a race that comes up empty forces another re-enumeration.
    pub fn acquire(self) -> Result<Held> {
        let mut last = None;
        for attempt in 1..=RACE_ATTEMPTS {
            if !self.force_reenumeration()? {
                debug!(
                    "{}: cannot re-enumerate; racing on the current device",
                    self.name
                );
            }
            match self.race() {
                Ok(held) => return Ok(held),
                Err(error) => {
                    debug!(
                        "{}: race attempt {attempt} came up empty: {error}",
                        self.name
                    );
                    last = Some(error);
                }
            }
        }
        Err(last.expect("the loop runs at least once")).with_context(|| {
            format!(
                "{}: {RACE_ATTEMPTS} re-enumerations all lost its audio interfaces to another \
                 process",
                self.name
            )
        })
    }

    /// Ask the DAC to re-enumerate, waiting for it to be on the bus first because a previous
    /// attempt may still have it off.
    ///
    /// `Ok(false)` when it is there but would not re-enumerate, which leaves the race to run
    /// on the device as it stands.
    fn force_reenumeration(&self) -> Result<bool> {
        let deadline = Instant::now() + ATTACH_TIMEOUT;
        loop {
            // SAFETY: the device ref is released before this leaves the block, on every path.
            // Opening the device itself is allowed even while usbaudiod holds the interfaces,
            // and that is enough to ask for a re-enumeration.
            let asked = unsafe {
                find_device_interface(self.vendor, self.product).map(|device| {
                    let result = reenumerate(device);
                    call!(device, Release);
                    result
                })
            };
            match asked {
                Some(Some(result)) => return ok(result, "USBDeviceReEnumerate").map(|()| true),
                Some(None) => return Ok(false),
                None if Instant::now() >= deadline => {
                    bail!("{} is not on the USB bus", self.name)
                }
                None => std::thread::sleep(RACE_POLL),
            }
        }
    }

    /// Poll for the interfaces and open them the instant they appear.
    fn race(&self) -> Result<Held> {
        let deadline = Instant::now() + RACE_TIMEOUT;
        let mut control: Option<InterfaceRef> = None;
        let mut streaming: Option<InterfaceRef> = None;
        let started = Instant::now();
        let mut polls = 0_u32;

        while Instant::now() < deadline && !(control.is_some() && streaming.is_some()) {
            polls += 1;
            // SAFETY: interfaces are released below on every exit path.
            unsafe {
                let Some(device) = find_device_interface(self.vendor, self.product) else {
                    std::thread::sleep(RACE_POLL);
                    continue;
                };
                if control.is_none() {
                    control = open_interface(device, self.native.control_interface);
                }
                if streaming.is_none() {
                    streaming = open_interface(device, self.native.streaming_interface);
                }
                if let (Some(control), Some(streaming)) = (control, streaming) {
                    debug!(
                        "{}: won the race in {polls} polls over {:.1} ms",
                        self.name,
                        started.elapsed().as_secs_f64() * 1000.0
                    );
                    return Ok(Held::claim(
                        device,
                        control,
                        streaming,
                        self.native,
                        self.name.clone(),
                    ));
                }
                call!(device, Release);
            }
            std::thread::sleep(RACE_POLL);
        }

        // SAFETY: close whatever half we won so the daemon can have the device back.
        unsafe {
            for interface in [control, streaming].into_iter().flatten() {
                call!(interface, USBInterfaceClose);
                call!(interface, Release);
            }
        }
        bail!(
            "still held after {:.1} s and {polls} polls",
            started.elapsed().as_secs_f64()
        )
    }
}

impl Drop for Dac {
    fn drop(&mut self) {
        // SAFETY: `service` is owned by this struct.
        unsafe { sys::IOObjectRelease(self.service) };
    }
}

/// Both audio interfaces of one DAC, held open so `usbaudiod` cannot take them back.
pub struct Held {
    device: DeviceRef,
    streaming: InterfaceRef,
    pub native: NativeDsd,
    pub name: String,
}

// The interfaces are driven from one thread at a time; the session owns them exclusively.
unsafe impl Send for Held {}

impl Held {
    /// Take ownership of the interfaces and record them where a signal handler can find
    /// them, so the DAC is handed back even on a path that runs no destructors.
    fn claim(
        device: DeviceRef,
        control: InterfaceRef,
        streaming: InterfaceRef,
        native: NativeDsd,
        name: String,
    ) -> Self {
        // Reaching here means `race` just opened both interfaces, which the previous holder
        // must therefore have closed. So an entry still sitting here belongs to a `Held`
        // that leaked, and handing that DAC back beats dropping its refs on the floor.
        release_claimed_dac();
        *CLAIMED.lock().unwrap_or_else(|error| error.into_inner()) = Some(Claim {
            device,
            control,
            streaming,
            name: name.clone(),
        });
        Self {
            device,
            streaming,
            native,
            name,
        }
    }

    /// Select the native DSD alternate setting and set the clock, returning what the DAC
    /// reports back. A DAC that accepts the request but reports a different rate has not
    /// done what was asked, so the readback is checked rather than assumed.
    pub fn configure(&self, dsd_rate: u32) -> Result<u32> {
        let frame_rate = dsd_rate / 32;
        // SAFETY: both interfaces are open for the lifetime of `self`.
        unsafe {
            ok(
                call!(
                    self.streaming,
                    SetAlternateInterface,
                    self.native.alt_setting
                ),
                "SetAlternateInterface",
            )?;
            self.set_clock(frame_rate)?;
            let readback = self.clock()?;
            if readback != frame_rate {
                bail!(
                    "{}: asked for {frame_rate} Hz but the clock reports {readback} Hz, so it \
                     cannot play this rate natively",
                    self.name
                );
            }
            Ok(readback)
        }
    }

    /// Release the alternate setting so the DAC returns to its PCM path.
    pub fn reset(&self) {
        // SAFETY: the interface is open until `Drop` runs.
        unsafe {
            call!(self.streaming, SetAlternateInterface, 0);
        }
    }

    unsafe fn set_clock(&self, hz: u32) -> Result<()> {
        let mut value = hz.to_le_bytes();
        let mut request = sys::IOUSBDevRequest {
            bmRequestType: REQUEST_TYPE_SET,
            bRequest: UAC2_CUR,
            wValue: CS_SAM_FREQ_CONTROL << 8,
            wIndex: (u16::from(self.native.clock_id) << 8)
                | u16::from(self.native.control_interface),
            wLength: 4,
            pData: value.as_mut_ptr().cast::<c_void>(),
            wLenDone: 0,
        };
        // SAFETY: `request` and its buffer outlive the synchronous call.
        unsafe {
            ok(
                call!(self.device, DeviceRequest, &raw mut request),
                "set sample clock",
            )
        }
    }

    unsafe fn clock(&self) -> Result<u32> {
        let mut value = [0_u8; 4];
        let mut request = sys::IOUSBDevRequest {
            bmRequestType: REQUEST_TYPE_GET,
            bRequest: UAC2_CUR,
            wValue: CS_SAM_FREQ_CONTROL << 8,
            wIndex: (u16::from(self.native.clock_id) << 8)
                | u16::from(self.native.control_interface),
            wLength: 4,
            pData: value.as_mut_ptr().cast::<c_void>(),
            wLenDone: 0,
        };
        // SAFETY: as above.
        unsafe {
            ok(
                call!(self.device, DeviceRequest, &raw mut request),
                "read sample clock",
            )?;
        }
        Ok(u32::from_le_bytes(value))
    }

    /// Pipe references for the data and feedback endpoints, which only exist once the
    /// alternate setting is active.
    pub fn pipes(&self) -> Result<(u8, Option<u8>)> {
        let mut data = None;
        let mut feedback = None;
        // SAFETY: the streaming interface is open and its alt setting selected.
        unsafe {
            let mut endpoints = 0_u8;
            ok(
                call!(self.streaming, GetNumEndpoints, &raw mut endpoints),
                "GetNumEndpoints",
            )?;
            for pipe in 1..=endpoints {
                let (mut direction, mut number, mut kind, mut interval) = (0_u8, 0_u8, 0_u8, 0_u8);
                let mut max_packet = 0_u16;
                let result = call!(
                    self.streaming,
                    GetPipeProperties,
                    pipe,
                    &raw mut direction,
                    &raw mut number,
                    &raw mut kind,
                    &raw mut max_packet,
                    &raw mut interval,
                );
                if result != sys::kIOReturnSuccess as sys::IOReturn
                    || u32::from(kind) != sys::kUSBIsoc
                {
                    continue;
                }
                if u32::from(direction) == sys::kUSBOut {
                    data = Some(pipe);
                } else {
                    feedback = Some(pipe);
                }
            }
        }
        let Some(data) = data else {
            bail!(
                "{}: native DSD alt setting exposes no isochronous output pipe",
                self.name
            );
        };
        Ok((data, feedback))
    }

    pub const fn streaming(&self) -> InterfaceRef {
        self.streaming
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        release_claimed_dac();
    }
}

/// The refs needed to hand one DAC back, kept where a signal handler can reach them.
struct Claim {
    device: DeviceRef,
    control: InterfaceRef,
    streaming: InterfaceRef,
    name: String,
}

// SAFETY: the refs are only ever touched under CLAIMED, and `release_claimed_dac` takes the
// entry before using them, so whichever thread wins releases each exactly once.
unsafe impl Send for Claim {}

static CLAIMED: Mutex<Option<Claim>> = Mutex::new(None);

/// Hand the claimed DAC back to `usbaudiod`, if one is claimed.
///
/// `Held::drop` runs this on the normal path, but a signal handler has to call it too:
/// `std::process::exit` runs no destructors, and a DAC that is never handed back stays
/// missing from Core Audio until it is physically replugged.
pub fn release_claimed_dac() {
    let claimed = CLAIMED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    let Some(claim) = claimed else {
        return;
    };
    // SAFETY: every ref was created in `race`, and taking the entry above means this runs
    // once for them.
    unsafe {
        call!(claim.streaming, SetAlternateInterface, 0);
        call!(claim.streaming, USBInterfaceClose);
        call!(claim.streaming, Release);
        call!(claim.control, USBInterfaceClose);
        call!(claim.control, Release);

        // Closing the interfaces is not enough on its own. usbaudiod only looks at a
        // device when it enumerates, so a device it lost stays lost: the DAC would
        // vanish from Core Audio until physically replugged. Re-enumerating puts it
        // through the normal matching path again and the daemon picks it back up.
        match reenumerate(claim.device) {
            Some(result) if result != sys::kIOReturnSuccess as sys::IOReturn => debug!(
                "{}: could not hand the device back: 0x{result:08x}",
                claim.name
            ),
            Some(_) => {}
            None => debug!(
                "{}: could not reopen the device to hand it back",
                claim.name
            ),
        }
        call!(claim.device, Release);
    }
}

/// Open the device, ask it to re-enumerate, and close it again. `None` when the device
/// could not be opened at all, which leaves it exactly as it was.
///
/// SAFETY: `device` must be a live device ref; it is not consumed.
unsafe fn reenumerate(device: DeviceRef) -> Option<sys::IOReturn> {
    unsafe {
        let mut opened = call!(device, USBDeviceOpen);
        if opened != sys::kIOReturnSuccess as sys::IOReturn {
            opened = call!(device, USBDeviceOpenSeize);
        }
        if opened != sys::kIOReturnSuccess as sys::IOReturn {
            return None;
        }
        let result = call!(device, USBDeviceReEnumerate, 0);
        call!(device, USBDeviceClose);
        Some(result)
    }
}

/// Build a device interface for one IOKit service.
unsafe fn device_interface(service: sys::io_service_t) -> Option<DeviceRef> {
    let mut plugin: *mut sys::IOCFPlugInInterface = ptr::null_mut();
    let mut plugin_ref = &raw mut plugin;
    let mut score = 0_i32;
    // SAFETY: the plugin is destroyed before returning.
    unsafe {
        let result = sys::IOCreatePlugInInterfaceForService(
            service,
            sys::uuid::device_user_client_type(),
            sys::uuid::plugin_interface(),
            &raw mut plugin_ref,
            &raw mut score,
        );
        if result != sys::kIOReturnSuccess as sys::IOReturn || plugin_ref.is_null() {
            return None;
        }
        let mut device: *mut c_void = ptr::null_mut();
        let query = (**plugin_ref)
            .QueryInterface
            .expect("plugin provides QueryInterface");
        query(
            plugin_ref.cast::<c_void>(),
            sys::CFUUIDGetUUIDBytes(sys::uuid::device_interface_500()),
            &raw mut device,
        );
        sys::IODestroyPlugInInterface(plugin_ref);
        if device.is_null() {
            return None;
        }
        Some(device.cast::<*mut sys::IOUSBDeviceInterface500>())
    }
}

/// A CoreFoundation number holding one 32-bit value, or null if it could not be made.
///
/// SAFETY: the caller owns the result and must release it.
unsafe fn cf_number(value: i32) -> sys::CFNumberRef {
    unsafe {
        sys::CFNumberCreate(
            sys::kCFAllocatorDefault,
            sys::kCFNumberSInt32Type as sys::CFNumberType,
            ptr::from_ref(&value).cast::<c_void>(),
        )
    }
}

/// A matching dictionary for one USB device, keyed by its vendor and product id.
///
/// The ids go into the dictionary rather than being compared afterwards because the race
/// runs this every poll: matching in the kernel returns one service, where matching on the
/// class alone returns every attached USB device and costs a plugin interface for each.
///
/// SAFETY: the result is consumed by `IOServiceGetMatchingServices`, or must be released.
unsafe fn matching_device(vendor: u16, product: u16) -> sys::CFMutableDictionaryRef {
    unsafe {
        let class = CString::new("IOUSBHostDevice").expect("literal has no interior nul");
        let matching = sys::IOServiceMatching(class.as_ptr());
        if matching.is_null() {
            return matching;
        }
        for (name, value) in [("idVendor", vendor), ("idProduct", product)] {
            let Ok(name) = CString::new(name) else {
                continue;
            };
            let key = sys::CFStringCreateWithCString(
                sys::kCFAllocatorDefault,
                name.as_ptr(),
                sys::kCFStringEncodingUTF8,
            );
            let number = cf_number(i32::from(value));
            if !key.is_null() && !number.is_null() {
                // The dictionary retains both, so the refs made here are released either way.
                sys::CFDictionarySetValue(matching, key.cast(), number.cast());
            }
            if !key.is_null() {
                sys::CFRelease(key.cast());
            }
            if !number.is_null() {
                sys::CFRelease(number.cast());
            }
        }
        matching
    }
}

/// Find a device by vendor and product id, returning an opened device interface.
unsafe fn find_device_interface(vendor: u16, product: u16) -> Option<DeviceRef> {
    // SAFETY: the matching dictionary is consumed; every service is released.
    unsafe {
        let matching = matching_device(vendor, product);
        if matching.is_null() {
            return None;
        }
        let mut iterator: sys::io_iterator_t = 0;
        if sys::IOServiceGetMatchingServices(0, matching, &raw mut iterator)
            != sys::kIOReturnSuccess as sys::IOReturn
        {
            return None;
        }
        let mut result = None;
        loop {
            let service = sys::IOIteratorNext(iterator);
            if service == 0 {
                break;
            }
            if result.is_none() {
                result = device_interface(service);
            }
            sys::IOObjectRelease(service);
        }
        sys::IOObjectRelease(iterator);
        result
    }
}

/// Open one interface of a device by its interface number, if it is free right now.
unsafe fn open_interface(device: DeviceRef, want: u8) -> Option<InterfaceRef> {
    let mut request = sys::IOUSBFindInterfaceRequest {
        bInterfaceClass: sys::kIOUSBFindInterfaceDontCare as u16,
        bInterfaceSubClass: sys::kIOUSBFindInterfaceDontCare as u16,
        bInterfaceProtocol: sys::kIOUSBFindInterfaceDontCare as u16,
        bAlternateSetting: sys::kIOUSBFindInterfaceDontCare as u16,
    };
    // SAFETY: every intermediate object is released on all paths.
    unsafe {
        let mut iterator: sys::io_iterator_t = 0;
        if call!(
            device,
            CreateInterfaceIterator,
            &raw mut request,
            &raw mut iterator
        ) != sys::kIOReturnSuccess as sys::IOReturn
        {
            return None;
        }
        let mut opened = None;
        loop {
            let node = sys::IOIteratorNext(iterator);
            if node == 0 {
                break;
            }
            if opened.is_none()
                && let Some(interface) = interface_interface(node)
            {
                let mut number = 0_u8;
                call!(interface, GetInterfaceNumber, &raw mut number);
                if number == want {
                    let mut result = call!(interface, USBInterfaceOpen);
                    if result != sys::kIOReturnSuccess as sys::IOReturn {
                        result = call!(interface, USBInterfaceOpenSeize);
                    }
                    if result == sys::kIOReturnSuccess as sys::IOReturn {
                        opened = Some(interface);
                    } else {
                        call!(interface, Release);
                    }
                } else {
                    call!(interface, Release);
                }
            }
            sys::IOObjectRelease(node);
        }
        sys::IOObjectRelease(iterator);
        opened
    }
}

unsafe fn interface_interface(node: sys::io_service_t) -> Option<InterfaceRef> {
    let mut plugin: *mut sys::IOCFPlugInInterface = ptr::null_mut();
    let mut plugin_ref = &raw mut plugin;
    let mut score = 0_i32;
    // SAFETY: the plugin is destroyed before returning.
    unsafe {
        let result = sys::IOCreatePlugInInterfaceForService(
            node,
            sys::uuid::interface_user_client_type(),
            sys::uuid::plugin_interface(),
            &raw mut plugin_ref,
            &raw mut score,
        );
        if result != sys::kIOReturnSuccess as sys::IOReturn || plugin_ref.is_null() {
            return None;
        }
        let mut interface: *mut c_void = ptr::null_mut();
        let query = (**plugin_ref)
            .QueryInterface
            .expect("plugin provides QueryInterface");
        query(
            plugin_ref.cast::<c_void>(),
            sys::CFUUIDGetUUIDBytes(sys::uuid::usb_interface_500()),
            &raw mut interface,
        );
        sys::IODestroyPlugInInterface(plugin_ref);
        if interface.is_null() {
            return None;
        }
        Some(interface.cast::<*mut sys::IOUSBInterfaceInterface500>())
    }
}

/// The active configuration descriptor, including every alternate setting.
unsafe fn config_descriptor<'a>(device: DeviceRef) -> Option<&'a [u8]> {
    let mut descriptor: sys::IOUSBConfigurationDescriptorPtr = ptr::null_mut();
    // SAFETY: the descriptor is owned by IOKit and lives as long as the device interface.
    unsafe {
        if call!(
            device,
            GetConfigurationDescriptorPtr,
            0,
            &raw mut descriptor
        ) != sys::kIOReturnSuccess as sys::IOReturn
            || descriptor.is_null()
        {
            return None;
        }
        let bytes = descriptor.cast::<u8>();
        // wTotalLength sits at offset 2, little endian.
        let total = u16::from_le_bytes([*bytes.add(2), *bytes.add(3)]) as usize;
        if total < 9 {
            return None;
        }
        Some(std::slice::from_raw_parts(bytes, total))
    }
}

/// Read a string property from the IO registry, for a human-readable device name.
fn registry_string(service: sys::io_service_t, key: &str) -> Option<String> {
    let key = CString::new(key).ok()?;
    // SAFETY: every CoreFoundation object created here is released before returning.
    unsafe {
        let name = sys::CFStringCreateWithCString(
            sys::kCFAllocatorDefault,
            key.as_ptr(),
            sys::kCFStringEncodingUTF8,
        );
        if name.is_null() {
            return None;
        }
        let value =
            sys::IORegistryEntryCreateCFProperty(service, name, sys::kCFAllocatorDefault, 0);
        sys::CFRelease(name.cast());
        if value.is_null() {
            return None;
        }
        let mut buffer = [0_i8; 256];
        let copied = sys::CFStringGetCString(
            value.cast(),
            buffer.as_mut_ptr(),
            buffer.len() as i64,
            sys::kCFStringEncodingUTF8,
        );
        sys::CFRelease(value);
        if copied == 0 {
            return None;
        }
        let bytes: Vec<u8> = buffer
            .iter()
            .take_while(|b| **b != 0)
            .map(|b| *b as u8)
            .collect();
        String::from_utf8(bytes).ok()
    }
}

/// Flatten a UAC2 range report into the discrete rates it allows.
///
/// A subrange with a resolution walks from min to max in steps; one with a zero resolution
/// is a single rate, which is how clock sources usually report a fixed rate list.
fn parse_clock_ranges(report: &[u8]) -> Vec<u32> {
    let Some(count) = report.first_chunk::<2>().map(|n| u16::from_le_bytes(*n)) else {
        return Vec::new();
    };
    let mut rates = Vec::new();
    for index in 0..count as usize {
        let start = 2 + index * SUBRANGE_BYTES;
        let Some(triple) = report.get(start..start + SUBRANGE_BYTES) else {
            break;
        };
        let value = |at: usize| {
            u32::from_le_bytes([triple[at], triple[at + 1], triple[at + 2], triple[at + 3]])
        };
        let (min, max, step) = (value(0), value(4), value(8));
        if step == 0 || min == max {
            rates.push(min);
            if max != min {
                rates.push(max);
            }
            continue;
        }
        let mut rate = min;
        while rate <= max {
            rates.push(rate);
            let Some(next) = rate.checked_add(step) else {
                break;
            };
            rate = next;
        }
    }
    rates.sort_unstable();
    rates.dedup();
    rates
}

#[cfg(test)]
mod tests {
    use crate::output::usb::device::{names_match, parse_clock_ranges};

    #[test]
    fn the_core_audio_name_and_the_usb_name_of_one_device_match_either_way_round() {
        assert!(names_match("Cayin RU7", "Cayin RU7 Playback"));
        assert!(names_match("Cayin RU7 Playback", "Cayin RU7"));
        // The fragment a user would actually type.
        assert!(names_match("Cayin RU7", "ru7"));
        assert!(names_match("Cayin RU7", "CAYIN"));
    }

    #[test]
    fn a_different_device_does_not_match_and_neither_does_an_empty_name() {
        assert!(!names_match("Cayin RU7", "MacBook Pro Speakers"));
        // An empty name is contained by every string, so it must not match anything.
        assert!(!names_match("", "Cayin RU7"));
        assert!(!names_match("Cayin RU7", ""));
        assert!(!names_match("Cayin RU7", "   "));
    }

    fn subrange(min: u32, max: u32, step: u32) -> Vec<u8> {
        let mut out = Vec::new();
        for field in [min, max, step] {
            out.extend_from_slice(&field.to_le_bytes());
        }
        out
    }

    #[test]
    fn discrete_rates_are_listed_once_each() {
        let mut report = 3_u16.to_le_bytes().to_vec();
        report.extend(subrange(44_100, 44_100, 0));
        report.extend(subrange(176_400, 176_400, 0));
        report.extend(subrange(352_800, 352_800, 0));

        assert_eq!(parse_clock_ranges(&report), [44_100, 176_400, 352_800]);
    }

    #[test]
    fn a_stepped_subrange_walks_from_min_to_max() {
        let mut report = 1_u16.to_le_bytes().to_vec();
        report.extend(subrange(44_100, 176_400, 44_100));

        assert_eq!(
            parse_clock_ranges(&report),
            [44_100, 88_200, 132_300, 176_400]
        );
    }

    #[test]
    fn a_report_shorter_than_it_claims_stops_at_what_is_there() {
        let mut report = 4_u16.to_le_bytes().to_vec();
        report.extend(subrange(352_800, 352_800, 0));

        assert_eq!(parse_clock_ranges(&report), [352_800]);
    }

    #[test]
    fn an_empty_report_yields_no_rates() {
        assert!(parse_clock_ranges(&[]).is_empty());
        assert!(parse_clock_ranges(&0_u16.to_le_bytes()).is_empty());
    }
}
