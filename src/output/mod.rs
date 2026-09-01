#[cfg(not(target_os = "macos"))]
compile_error!("dsd-rust currently supports macOS Core Audio only");

pub mod encoding;
pub mod hal;
pub mod stream;

use anyhow::{Result, bail};

use crate::output::hal::Device;
use crate::output::stream::supported_dop_rates;

/// What the `devices` command shows for one output device.
pub struct DeviceSummary {
    pub name: String,
    pub uid: String,
    pub is_default: bool,
    pub current_rate: f64,
    pub dop_rates: Vec<u32>,
}

pub fn list_devices() -> Result<Vec<DeviceSummary>> {
    let default = Device::default_output().ok();
    let mut summaries = Vec::new();
    for device in Device::all()? {
        summaries.push(DeviceSummary {
            name: device.name().unwrap_or_else(|_| "<unnamed>".to_owned()),
            uid: device.uid().unwrap_or_default(),
            is_default: default == Some(device),
            current_rate: device.nominal_sample_rate().unwrap_or(0.0),
            dop_rates: supported_dop_rates(&device),
        });
    }
    Ok(summaries)
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
