//! Thin safe wrappers over the Core Audio HAL property API.

use std::mem::MaybeUninit;
use std::os::raw::{c_char, c_void};

use anyhow::{Result, bail};
use coreaudio_sys::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectHasProperty,
    AudioObjectID, AudioObjectIsPropertySettable, AudioObjectPropertyAddress,
    AudioObjectSetPropertyData, AudioStreamBasicDescription, AudioStreamRangedDescription,
    CFRelease, CFStringGetCString, CFStringGetLength, CFStringGetMaximumSizeForEncoding,
    CFStringRef, OSStatus, kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyHogMode, kAudioDevicePropertyNominalSampleRate,
    kAudioDevicePropertyStreams, kAudioDevicePropertySupportsMixing,
    kAudioDevicePropertyVolumeScalar, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain, kAudioObjectPropertyName,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    kAudioStreamPropertyAvailablePhysicalFormats, kAudioStreamPropertyPhysicalFormat,
    kAudioStreamPropertyVirtualFormat, kCFStringEncodingUTF8,
};

pub const GLOBAL: u32 = kAudioObjectPropertyScopeGlobal;
pub const OUTPUT: u32 = kAudioObjectPropertyScopeOutput;

pub const fn address(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// Render an `OSStatus` the way Core Audio documents it: a four character code when printable.
pub fn status_text(status: OSStatus) -> String {
    let bytes = (status as u32).to_be_bytes();
    let printable = bytes.iter().all(|byte| (0x20..0x7F).contains(byte));
    if printable {
        format!("'{}' ({status})", String::from_utf8_lossy(&bytes))
    } else {
        format!("{status}")
    }
}

fn check(status: OSStatus, what: &str) -> Result<()> {
    if status == 0 {
        return Ok(());
    }
    bail!("{what} failed: {}", status_text(status))
}

pub fn has(id: AudioObjectID, address: &AudioObjectPropertyAddress) -> bool {
    unsafe { AudioObjectHasProperty(id, address) != 0 }
}

pub fn is_settable(id: AudioObjectID, address: &AudioObjectPropertyAddress) -> bool {
    let mut settable = 0;
    let status = unsafe { AudioObjectIsPropertySettable(id, address, &mut settable) };
    status == 0 && settable != 0
}

pub fn get<T: Copy>(id: AudioObjectID, address: &AudioObjectPropertyAddress) -> Result<T> {
    let mut value = MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            address,
            0,
            std::ptr::null(),
            &mut size,
            value.as_mut_ptr().cast::<c_void>(),
        )
    };
    check(status, "AudioObjectGetPropertyData")?;
    Ok(unsafe { value.assume_init() })
}

pub fn get_all<T: Copy>(id: AudioObjectID, address: &AudioObjectPropertyAddress) -> Result<Vec<T>> {
    let mut bytes = 0_u32;
    let status =
        unsafe { AudioObjectGetPropertyDataSize(id, address, 0, std::ptr::null(), &mut bytes) };
    check(status, "AudioObjectGetPropertyDataSize")?;

    let count = bytes as usize / size_of::<T>();
    let mut values = Vec::<T>::with_capacity(count);
    if count == 0 {
        return Ok(values);
    }
    let mut size = bytes;
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            address,
            0,
            std::ptr::null(),
            &mut size,
            values.as_mut_ptr().cast::<c_void>(),
        )
    };
    check(status, "AudioObjectGetPropertyData")?;
    unsafe { values.set_len(size as usize / size_of::<T>()) };
    Ok(values)
}

pub fn set<T: Copy>(
    id: AudioObjectID,
    address: &AudioObjectPropertyAddress,
    value: &T,
) -> Result<()> {
    let status = unsafe {
        AudioObjectSetPropertyData(
            id,
            address,
            0,
            std::ptr::null(),
            size_of::<T>() as u32,
            std::ptr::from_ref(value).cast::<c_void>(),
        )
    };
    check(status, "AudioObjectSetPropertyData")
}

pub fn get_string(id: AudioObjectID, address: &AudioObjectPropertyAddress) -> Result<String> {
    let cfstring: CFStringRef = get(id, address)?;
    if cfstring.is_null() {
        bail!("property returned a null string");
    }
    let text = unsafe {
        let capacity =
            CFStringGetMaximumSizeForEncoding(CFStringGetLength(cfstring), kCFStringEncodingUTF8)
                + 1;
        let mut buffer = vec![0 as c_char; capacity as usize];
        let copied = CFStringGetCString(
            cfstring,
            buffer.as_mut_ptr(),
            capacity,
            kCFStringEncodingUTF8,
        );
        CFRelease(cfstring.cast());
        if copied == 0 {
            bail!("could not decode a Core Audio string property");
        }
        std::ffi::CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    };
    Ok(text)
}

/// An output-capable audio device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device(pub AudioObjectID);

impl Device {
    pub fn all() -> Result<Vec<Self>> {
        let address = address(kAudioHardwarePropertyDevices, GLOBAL);
        let ids: Vec<AudioObjectID> = get_all(kAudioObjectSystemObject, &address)?;
        let mut devices = Vec::new();
        for id in ids {
            let device = Self(id);
            if device
                .output_streams()
                .is_ok_and(|streams| !streams.is_empty())
            {
                devices.push(device);
            }
        }
        Ok(devices)
    }

    pub fn default_output() -> Result<Self> {
        let address = address(kAudioHardwarePropertyDefaultOutputDevice, GLOBAL);
        Ok(Self(get(kAudioObjectSystemObject, &address)?))
    }

    pub fn name(&self) -> Result<String> {
        get_string(self.0, &address(kAudioObjectPropertyName, GLOBAL))
    }

    pub fn uid(&self) -> Result<String> {
        get_string(self.0, &address(kAudioDevicePropertyDeviceUID, GLOBAL))
    }

    pub fn output_streams(&self) -> Result<Vec<Stream>> {
        let ids: Vec<AudioObjectID> =
            get_all(self.0, &address(kAudioDevicePropertyStreams, OUTPUT))?;
        Ok(ids.into_iter().map(Stream).collect())
    }

    pub fn nominal_sample_rate(&self) -> Result<f64> {
        get(
            self.0,
            &address(kAudioDevicePropertyNominalSampleRate, GLOBAL),
        )
    }

    pub fn set_nominal_sample_rate(&self, rate: f64) -> Result<()> {
        set(
            self.0,
            &address(kAudioDevicePropertyNominalSampleRate, GLOBAL),
            &rate,
        )
    }

    pub fn buffer_frame_size(&self) -> Result<u32> {
        get(
            self.0,
            &address(kAudioDevicePropertyBufferFrameSize, OUTPUT),
        )
    }

    pub fn set_buffer_frame_size(&self, frames: u32) -> Result<()> {
        set(
            self.0,
            &address(kAudioDevicePropertyBufferFrameSize, OUTPUT),
            &frames,
        )
    }

    /// Claim the device for this process so nothing else can mix into it.
    pub fn set_hog_mode(&self, pid: libc::pid_t) -> Result<()> {
        set(self.0, &address(kAudioDevicePropertyHogMode, OUTPUT), &pid)
    }

    /// Ask the device to hand the IOProc the raw stream format instead of mixed float.
    pub fn set_mixing(&self, enabled: bool) -> Result<()> {
        let address = address(kAudioDevicePropertySupportsMixing, GLOBAL);
        if !has(self.0, &address) || !is_settable(self.0, &address) {
            bail!("device does not expose a mixing switch");
        }
        set(self.0, &address, &u32::from(enabled))
    }

    pub fn supports_mixing_switch(&self) -> bool {
        let address = address(kAudioDevicePropertySupportsMixing, GLOBAL);
        has(self.0, &address) && is_settable(self.0, &address)
    }

    /// Master output volume, when the device exposes one.
    pub fn volume_scalar(&self) -> Option<f32> {
        let address = address(kAudioDevicePropertyVolumeScalar, OUTPUT);
        if !has(self.0, &address) {
            return None;
        }
        get(self.0, &address).ok()
    }
}

/// One output stream of a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stream(pub AudioObjectID);

impl Stream {
    pub fn available_physical_formats(&self) -> Result<Vec<AudioStreamRangedDescription>> {
        get_all(
            self.0,
            &address(kAudioStreamPropertyAvailablePhysicalFormats, GLOBAL),
        )
    }

    pub fn physical_format(&self) -> Result<AudioStreamBasicDescription> {
        get(self.0, &address(kAudioStreamPropertyPhysicalFormat, GLOBAL))
    }

    pub fn set_physical_format(&self, format: &AudioStreamBasicDescription) -> Result<()> {
        set(
            self.0,
            &address(kAudioStreamPropertyPhysicalFormat, GLOBAL),
            format,
        )
    }

    pub fn virtual_format(&self) -> Result<AudioStreamBasicDescription> {
        get(self.0, &address(kAudioStreamPropertyVirtualFormat, GLOBAL))
    }

    pub fn set_virtual_format(&self, format: &AudioStreamBasicDescription) -> Result<()> {
        set(
            self.0,
            &address(kAudioStreamPropertyVirtualFormat, GLOBAL),
            format,
        )
    }
}
