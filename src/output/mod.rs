#[cfg(not(target_os = "macos"))]
compile_error!("dsd-rust currently supports macOS Core Audio only");

pub mod encoding;
pub mod hal;
pub mod stream;
pub mod usb;

use std::fmt;

use anyhow::{Result, bail};
use coreaudio_sys::{
    AudioStreamBasicDescription, AudioStreamRangedDescription, kAudioFormatFlagIsAlignedHigh,
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved,
    kAudioFormatFlagIsNonMixable, kAudioFormatFlagIsPacked, kAudioFormatFlagIsSignedInteger,
    kAudioFormatLinearPCM,
};

use crate::dsd::DsdRate;
use crate::output::hal::Device;
use crate::output::stream::supported_dop_rates;
use crate::output::usb::device::Dac;

/// What the `devices` command shows for one output device.
pub struct DeviceSummary {
    pub name: String,
    pub uid: String,
    pub is_default: bool,
    pub current_rate: f64,
    pub dop_rates: Vec<u32>,
    pub hog_owner: Option<i32>,
    /// DSD rates this device plays natively, when it advertises a RAW_DATA alternate
    /// setting. Independent of `dop_rates`, and usually reaching higher.
    pub native_dsd: Vec<u32>,
}

pub fn list_devices() -> Result<Vec<DeviceSummary>> {
    let default = Device::default_output().ok();
    let native = Dac::discover().unwrap_or_default();
    let mut summaries = Vec::new();
    for device in Device::all()? {
        let name = device.name().unwrap_or_else(|_| "<unnamed>".to_owned());
        summaries.push(DeviceSummary {
            native_dsd: native_rates(&native, &name),
            name,
            uid: device.uid().unwrap_or_default(),
            is_default: default == Some(device),
            current_rate: device.nominal_sample_rate().unwrap_or(0.0),
            dop_rates: supported_dop_rates(&device),
            hog_owner: device.hog_owner(),
        });
    }
    Ok(summaries)
}

/// DSD rates one device plays natively.
///
/// The clock is shared with the PCM path, so its range report lists PCM rates too. A frame
/// rate counts here only when 32 DSD bits per frame lands on a real DSD rate, which is what
/// separates 352800 Hz carrying DSD256 from 352800 Hz carrying PCM.
fn native_rates(dacs: &[Dac], core_audio_name: &str) -> Vec<u32> {
    let haystack = core_audio_name.to_lowercase();
    // Core Audio and USB name the same device differently -- "Cayin RU7 Playback" against
    // "Cayin RU7" -- so match whichever name contains the other.
    let Some(dac) = dacs.iter().find(|dac| {
        let usb = dac.name.to_lowercase();
        !usb.is_empty() && (haystack.contains(&usb) || usb.contains(&haystack))
    }) else {
        return Vec::new();
    };
    let Ok(clock) = dac.clock_rates() else {
        return Vec::new();
    };

    let bandwidth = dac.native.max_dsd_rate(2);
    let mut rates = Vec::new();
    for frame in clock {
        let hz = frame.saturating_mul(32);
        if hz > bandwidth {
            continue;
        }
        if DsdRate::new(hz).multiplier().is_some_and(|n| n >= 64) {
            rates.push(hz);
        }
    }
    rates
}

/// One line describing a stream format, as the driver advertises it.
pub struct FormatLine(pub AudioStreamBasicDescription);

impl fmt::Display for FormatLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let format = self.0;
        if format.mFormatID != kAudioFormatLinearPCM {
            let id = format.mFormatID.to_be_bytes();
            return write!(f, "{} non-PCM", String::from_utf8_lossy(&id));
        }
        let flags = format.mFormatFlags;
        let channels = format.mChannelsPerFrame.max(1);
        let sample_bytes = format.mBytesPerFrame / channels;
        let kind = if flags & kAudioFormatFlagIsFloat != 0 {
            "float"
        } else if flags & kAudioFormatFlagIsSignedInteger != 0 {
            "int"
        } else {
            "uint"
        };
        write!(
            f,
            "{:>7.0} Hz  {:>2} bit {kind} in {sample_bytes} B  {channels} ch  {}",
            format.mSampleRate,
            format.mBitsPerChannel,
            if flags & kAudioFormatFlagIsBigEndian != 0 {
                "BE"
            } else {
                "LE"
            }
        )?;
        for (flag, name) in [
            (kAudioFormatFlagIsPacked, "packed"),
            (kAudioFormatFlagIsAlignedHigh, "aligned-high"),
            (kAudioFormatFlagIsNonInterleaved, "non-interleaved"),
            (kAudioFormatFlagIsNonMixable, "non-mixable"),
        ] {
            if flags & flag != 0 {
                write!(f, " {name}")?;
            }
        }
        Ok(())
    }
}

/// Every format one output stream advertises, for `devices --formats`.
pub struct StreamFormats {
    pub current_physical: AudioStreamBasicDescription,
    pub current_virtual: AudioStreamBasicDescription,
    pub physical: Vec<AudioStreamRangedDescription>,
    pub virtual_formats: Vec<AudioStreamRangedDescription>,
}

pub fn stream_formats(device: &Device) -> Result<Vec<StreamFormats>> {
    let mut streams = Vec::new();
    for stream in device.output_streams()? {
        streams.push(StreamFormats {
            current_physical: stream.physical_format()?,
            current_virtual: stream.virtual_format()?,
            physical: stream.available_physical_formats().unwrap_or_default(),
            virtual_formats: stream.available_virtual_formats().unwrap_or_default(),
        });
    }
    Ok(streams)
}

/// Resolve a device by name fragment or UID, falling back to the system default.
pub fn find_device(query: Option<&str>) -> Result<(Device, String)> {
    let Some(query) = query else {
        let device = Device::default_output()?;
        let name = device.name().unwrap_or_else(|_| "<unnamed>".to_owned());
        return Ok((device, name));
    };

    let needle = query.to_lowercase();
    let mut names = Vec::new();
    for device in Device::all()? {
        let name = device.name().unwrap_or_default();
        if name.to_lowercase().contains(&needle) || device.uid().unwrap_or_default() == query {
            return Ok((device, name));
        }
        names.push(name);
    }
    bail!(
        "no output device matches {query:?}; available: {}",
        names.join(", ")
    )
}
