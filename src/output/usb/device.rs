//! Taking a USB DAC away from `usbaudiod` and configuring its native DSD path.
//!
//! macOS runs USB audio in a userspace daemon, not a kernel driver, so the audio interfaces
//! are held by another process rather than owned by the kernel. Nothing can seize them while
//! that daemon holds them, but the daemon has to re-acquire them after the device
//! re-enumerates, and that leaves a window of roughly 20 ms. This module forces a
//! re-enumeration, wins that race, and then simply never lets go: the interfaces stay open
//! for the life of the session, which keeps the daemon out.

use std::ffi::{CString, c_void};
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tracing::debug;

use crate::output::usb::descriptors::{NativeDsd, find_native_dsd};
use crate::output::usb::sys;

type DeviceRef = *mut *mut sys::IOUSBDeviceInterface500;
type InterfaceRef = *mut *mut sys::IOUSBInterfaceInterface500;

/// How long to keep racing before giving up on a re-enumeration.
const RACE_TIMEOUT: Duration = Duration::from_secs(10);
const RACE_POLL: Duration = Duration::from_micros(500);

/// UAC2 class request: set the current value of a control.
const UAC2_CUR: u8 = 0x01;
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

    /// Force a re-enumeration, win the race for both audio interfaces, and hold them.
    pub fn acquire(self) -> Result<Held> {
        // SAFETY: every raw pointer below is checked before use and released on every path.
        unsafe {
            let Some(device) = device_interface(self.service) else {
                bail!("{}: cannot open a USB device interface", self.name);
            };

            // Opening the device itself is allowed even while usbaudiod holds the
            // interfaces, and that is enough to ask for a re-enumeration.
            let mut opened = call!(device, USBDeviceOpen);
            if opened != sys::kIOReturnSuccess as sys::IOReturn {
                opened = call!(device, USBDeviceOpenSeize);
            }
            if opened == sys::kIOReturnSuccess as sys::IOReturn {
                let reenumerated = call!(device, USBDeviceReEnumerate, 0);
                call!(device, USBDeviceClose);
                ok(reenumerated, "USBDeviceReEnumerate")?;
            } else {
                debug!(
                    "{}: cannot re-enumerate; racing on the current device",
                    self.name
                );
            }
            call!(device, Release);

            let held = self.race()?;
            Ok(held)
        }
    }

    /// Poll for the interfaces and open them the instant they appear.
    fn race(&self) -> Result<Held> {
        let deadline = Instant::now() + RACE_TIMEOUT;
        let mut control: Option<InterfaceRef> = None;
        let mut streaming: Option<InterfaceRef> = None;
        let started = Instant::now();

        while Instant::now() < deadline && !(control.is_some() && streaming.is_some()) {
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
                    return Ok(Held {
                        device,
                        control,
                        streaming,
                        native: self.native,
                        name: self.name.clone(),
                    });
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
            "{}: lost the race for its audio interfaces after {:.1} s; another process \
             claimed them first. Try again.",
            self.name,
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
    control: InterfaceRef,
    streaming: InterfaceRef,
    pub native: NativeDsd,
    pub name: String,
}

// The interfaces are driven from one thread at a time; the session owns them exclusively.
unsafe impl Send for Held {}

impl Held {
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
        // SAFETY: every pointer here was created in `race` and is released exactly once.
        unsafe {
            call!(self.streaming, SetAlternateInterface, 0);
            call!(self.streaming, USBInterfaceClose);
            call!(self.streaming, Release);
            call!(self.control, USBInterfaceClose);
            call!(self.control, Release);

            // Closing the interfaces is not enough on its own. usbaudiod only looks at a
            // device when it enumerates, so a device it lost stays lost: the DAC would
            // vanish from Core Audio until physically replugged. Re-enumerating puts it
            // through the normal matching path again and the daemon picks it back up.
            let mut opened = call!(self.device, USBDeviceOpen);
            if opened != sys::kIOReturnSuccess as sys::IOReturn {
                opened = call!(self.device, USBDeviceOpenSeize);
            }
            if opened == sys::kIOReturnSuccess as sys::IOReturn {
                let result = call!(self.device, USBDeviceReEnumerate, 0);
                call!(self.device, USBDeviceClose);
                if result != sys::kIOReturnSuccess as sys::IOReturn {
                    debug!(
                        "{}: could not hand the device back: 0x{result:08x}",
                        self.name
                    );
                }
            }
            call!(self.device, Release);
        }
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

/// Find a device by vendor and product id, returning an opened device interface.
unsafe fn find_device_interface(vendor: u16, product: u16) -> Option<DeviceRef> {
    // SAFETY: the matching dictionary is consumed; every service is released.
    unsafe {
        let class = CString::new("IOUSBHostDevice").expect("literal has no interior nul");
        let matching = sys::IOServiceMatching(class.as_ptr());
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
            if result.is_none()
                && let Some(device) = device_interface(service)
            {
                let mut this_vendor = 0;
                let mut this_product = 0;
                call!(device, GetDeviceVendor, &raw mut this_vendor);
                call!(device, GetDeviceProduct, &raw mut this_product);
                if this_vendor == vendor && this_product == product {
                    result = Some(device);
                } else {
                    call!(device, Release);
                }
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
