//! Finding the native DSD path in a UAC2 configuration descriptor.
//!
//! A DAC that plays native DSD advertises an extra alternate setting on its audio streaming
//! interface whose format is `RAW_DATA` rather than PCM. Core Audio ignores that setting
//! because it is not PCM, which is exactly why the rate is unreachable through the HAL.

use anyhow::{Result, bail};

/// Class-specific interface descriptor.
const CS_INTERFACE: u8 = 0x24;
const DESC_INTERFACE: u8 = 0x04;
const DESC_ENDPOINT: u8 = 0x05;

const AUDIO_CLASS: u8 = 0x01;
const SUBCLASS_CONTROL: u8 = 0x01;
const SUBCLASS_STREAMING: u8 = 0x02;

const AC_CLOCK_SOURCE: u8 = 0x0A;
const AS_GENERAL: u8 = 0x01;
const AS_FORMAT_TYPE: u8 = 0x02;

/// `bmFormats` bit 31: the subslots carry opaque bytes, not PCM samples.
const FORMAT_TYPE_I_RAW_DATA: u32 = 1 << 31;
/// Native DSD packs 32 DSD bits per channel per frame.
const NATIVE_SUBSLOT_BYTES: u8 = 4;

/// Endpoint transfer type field of `bmAttributes`.
const TRANSFER_TYPE_MASK: u8 = 0x03;
const TRANSFER_ISOCHRONOUS: u8 = 0x01;
const ENDPOINT_DIRECTION_IN: u8 = 0x80;

/// Both the sampling frequency control and its range must be host programmable.
const CLOCK_FREQ_HOST_PROGRAMMABLE: u8 = 0x03;

/// Everything needed to drive one DAC's native DSD path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeDsd {
    pub control_interface: u8,
    pub streaming_interface: u8,
    pub alt_setting: u8,
    pub clock_id: u8,
    pub out_endpoint: u8,
    pub feedback_endpoint: Option<u8>,
    pub max_packet: u16,
}

impl NativeDsd {
    /// Largest DSD rate this endpoint can carry, in bits per second per channel.
    pub const fn max_dsd_rate(self, channels: u32) -> u32 {
        // 8000 microframes per second, 8 DSD bits per byte.
        (self.max_packet as u32 / channels / NATIVE_SUBSLOT_BYTES as u32) * 32 * 8000
    }
}

/// One descriptor in the configuration blob.
struct Descriptor<'a> {
    kind: u8,
    body: &'a [u8],
}

/// Walk a configuration descriptor, yielding each TLV. Stops at the first malformed length
/// rather than guessing, so a truncated blob cannot be read past its end.
fn walk(config: &[u8]) -> impl Iterator<Item = Descriptor<'_>> {
    let mut offset = 0;
    std::iter::from_fn(move || {
        if offset + 2 > config.len() {
            return None;
        }
        let length = config[offset] as usize;
        if length < 2 || offset + length > config.len() {
            return None;
        }
        let kind = config[offset + 1];
        let body = &config[offset + 2..offset + length];
        offset += length;
        Some(Descriptor { kind, body })
    })
}

/// What an interface descriptor says about itself, less the fields we never read.
#[derive(Clone, Copy)]
struct Interface {
    number: u8,
    alt_setting: u8,
    class: u8,
    subclass: u8,
}

impl Interface {
    fn parse(body: &[u8]) -> Option<Self> {
        // bInterfaceNumber, bAlternateSetting, bNumEndpoints, bInterfaceClass, bInterfaceSubClass
        if body.len() < 5 {
            return None;
        }
        Some(Self {
            number: body[0],
            alt_setting: body[1],
            class: body[3],
            subclass: body[4],
        })
    }

    const fn is_audio_control(self) -> bool {
        self.class == AUDIO_CLASS && self.subclass == SUBCLASS_CONTROL
    }

    const fn is_audio_streaming(self) -> bool {
        self.class == AUDIO_CLASS && self.subclass == SUBCLASS_STREAMING
    }
}

/// An alternate setting that carries raw DSD, once its format descriptors agree.
#[derive(Default, Clone, Copy)]
struct Candidate {
    interface: u8,
    alt_setting: u8,
    raw_data: bool,
    subslot_bytes: u8,
    out_endpoint: Option<u8>,
    feedback_endpoint: Option<u8>,
    max_packet: u16,
}

impl Candidate {
    const fn is_native_dsd(&self) -> bool {
        self.raw_data && self.subslot_bytes == NATIVE_SUBSLOT_BYTES && self.out_endpoint.is_some()
    }
}

/// Locate the native DSD alternate setting and the clock that drives it.
pub fn find_native_dsd(config: &[u8]) -> Result<NativeDsd> {
    let mut control_interface = None;
    let mut clock_id = None;
    let mut clock_fallback = None;
    let mut current: Option<Interface> = None;
    let mut candidate = Candidate::default();
    let mut found: Option<Candidate> = None;

    for descriptor in walk(config) {
        match descriptor.kind {
            DESC_INTERFACE => {
                if candidate.is_native_dsd() && found.is_none() {
                    found = Some(candidate);
                }
                let interface = Interface::parse(descriptor.body);
                candidate = Candidate::default();
                if let Some(interface) = interface {
                    if interface.is_audio_control() {
                        control_interface = Some(interface.number);
                    }
                    candidate.interface = interface.number;
                    candidate.alt_setting = interface.alt_setting;
                }
                current = interface;
            }
            CS_INTERFACE => {
                let Some(interface) = current else { continue };
                let Some((&subtype, fields)) = descriptor.body.split_first() else {
                    continue;
                };
                if interface.is_audio_control() && subtype == AC_CLOCK_SOURCE {
                    // bClockID, bmAttributes, bmControls
                    if fields.len() >= 3 {
                        clock_fallback.get_or_insert(fields[0]);
                        if fields[2] & CLOCK_FREQ_HOST_PROGRAMMABLE == CLOCK_FREQ_HOST_PROGRAMMABLE
                        {
                            clock_id.get_or_insert(fields[0]);
                        }
                    }
                } else if interface.is_audio_streaming() {
                    let _ = read_streaming_descriptor(subtype, fields, &mut candidate);
                }
            }
            DESC_ENDPOINT => read_endpoint(descriptor.body, &mut candidate),
            _ => {}
        }
    }
    if candidate.is_native_dsd() && found.is_none() {
        found = Some(candidate);
    }

    let Some(found) = found else {
        bail!("device advertises no native DSD alternate setting (no RAW_DATA format)");
    };
    let Some(control_interface) = control_interface else {
        bail!("device has no audio control interface to set the sample clock on");
    };
    let Some(clock_id) = clock_id.or(clock_fallback) else {
        bail!("device has no clock source, so its sample rate cannot be set");
    };
    Ok(NativeDsd {
        control_interface,
        streaming_interface: found.interface,
        alt_setting: found.alt_setting,
        clock_id,
        out_endpoint: found.out_endpoint.expect("checked by is_native_dsd"),
        feedback_endpoint: found.feedback_endpoint,
        max_packet: found.max_packet,
    })
}

/// Returns `None` for a descriptor too short to hold the field being read, which leaves the
/// candidate as it was rather than guessing at a truncated format.
fn read_streaming_descriptor(subtype: u8, fields: &[u8], candidate: &mut Candidate) -> Option<()> {
    match subtype {
        // bTerminalLink, bmControls, bFormatType, bmFormats(4)
        AS_GENERAL => {
            let formats: [u8; 4] = fields.get(3..7)?.try_into().ok()?;
            candidate.raw_data = u32::from_le_bytes(formats) & FORMAT_TYPE_I_RAW_DATA != 0;
        }
        // bFormatType, bSubslotSize, bBitResolution
        AS_FORMAT_TYPE => candidate.subslot_bytes = *fields.get(1)?,
        _ => {}
    }
    Some(())
}

fn read_endpoint(body: &[u8], candidate: &mut Candidate) {
    // bEndpointAddress, bmAttributes, wMaxPacketSize(2), bInterval
    if body.len() < 4 {
        return;
    }
    let address = body[0];
    if body[1] & TRANSFER_TYPE_MASK != TRANSFER_ISOCHRONOUS {
        return;
    }
    if address & ENDPOINT_DIRECTION_IN == 0 {
        candidate.out_endpoint = Some(address);
        candidate.max_packet = u16::from_le_bytes([body[2], body[3]]);
    } else {
        candidate.feedback_endpoint = Some(address);
    }
}

#[cfg(test)]
mod tests {
    use crate::output::usb::descriptors::{NativeDsd, find_native_dsd};

    fn interface(number: u8, alt: u8, endpoints: u8, subclass: u8) -> Vec<u8> {
        vec![9, 0x04, number, alt, endpoints, 0x01, subclass, 0x20, 0]
    }

    fn endpoint(address: u8, attributes: u8, max_packet: u16) -> Vec<u8> {
        let [lo, hi] = max_packet.to_le_bytes();
        vec![7, 0x05, address, attributes, lo, hi, 1]
    }

    /// The Cayin RU7's real configuration descriptor, transcribed from its USB descriptors:
    /// four PCM alternate settings and one RAW_DATA setting for native DSD.
    fn ru7() -> Vec<u8> {
        let mut config = vec![9, 0x02, 0, 0, 2, 1, 0, 0x80, 50];

        config.extend(interface(0, 0, 0, 0x01));
        // Audio control: header, clock source (ID 5, host programmable), terminals.
        config.extend([9, 0x24, 0x01, 0x00, 0x02, 0x04, 0x40, 0x00, 0x00]);
        config.extend([8, 0x24, 0x0A, 0x05, 0x03, 0x07, 0x00, 0x00]);
        config.extend([
            17, 0x24, 0x02, 0x01, 0x01, 0x01, 0x00, 0x05, 0x02, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ]);

        config.extend(interface(1, 0, 0, 0x02));
        for (alt, subslot, bits) in [(1_u8, 2_u8, 16_u8), (2, 3, 24), (3, 4, 32)] {
            config.extend(interface(1, alt, 2, 0x02));
            config.extend([
                16, 0x24, 0x01, 0x01, 0x05, 0x01, 0x01, 0x00, 0x00, 0x00, 0x02, 0x03, 0x00, 0x00,
                0x00, 0x00,
            ]);
            config.extend([6, 0x24, 0x02, 0x01, subslot, bits]);
            config.extend(endpoint(0x01, 0x05, 776));
            config.extend(endpoint(0x81, 0x11, 4));
        }
        // Alt 4: bmFormats = 0x80000000, RAW_DATA.
        config.extend(interface(1, 4, 2, 0x02));
        config.extend([
            16, 0x24, 0x01, 0x01, 0x05, 0x01, 0x00, 0x00, 0x00, 0x80, 0x02, 0x03, 0x00, 0x00, 0x00,
            0x00,
        ]);
        config.extend([6, 0x24, 0x02, 0x01, 0x04, 0x20]);
        config.extend(endpoint(0x01, 0x05, 776));
        config.extend(endpoint(0x81, 0x11, 4));
        config
    }

    #[test]
    fn the_ru7_native_dsd_path_is_found() {
        let found = find_native_dsd(&ru7()).expect("RU7 advertises native DSD");

        assert_eq!(
            found,
            NativeDsd {
                control_interface: 0,
                streaming_interface: 1,
                alt_setting: 4,
                clock_id: 5,
                out_endpoint: 0x01,
                feedback_endpoint: Some(0x81),
                max_packet: 776,
            }
        );
    }

    #[test]
    fn the_ru7_endpoint_has_room_for_dsd256_but_not_dsd1024() {
        let found = find_native_dsd(&ru7()).expect("RU7 advertises native DSD");

        // 776 bytes per microframe over two channels is 97 subslots, so 24.832 MHz.
        assert_eq!(found.max_dsd_rate(2), 24_832_000);
        assert!(found.max_dsd_rate(2) >= 22_579_200, "reaches DSD512");
        assert!(found.max_dsd_rate(2) < 45_158_400, "but not DSD1024");
    }

    #[test]
    fn a_pcm_only_device_is_rejected_with_a_reason() {
        let mut config = vec![9, 0x02, 0, 0, 2, 1, 0, 0x80, 50];
        config.extend(interface(0, 0, 0, 0x01));
        config.extend([8, 0x24, 0x0A, 0x05, 0x03, 0x07, 0x00, 0x00]);
        config.extend(interface(1, 1, 2, 0x02));
        config.extend([
            16, 0x24, 0x01, 0x01, 0x05, 0x01, 0x01, 0x00, 0x00, 0x00, 0x02, 0x03, 0x00, 0x00, 0x00,
            0x00,
        ]);
        config.extend([6, 0x24, 0x02, 0x01, 0x04, 0x20]);
        config.extend(endpoint(0x01, 0x05, 776));

        let error = find_native_dsd(&config).expect_err("PCM only");

        assert!(error.to_string().contains("no native DSD"), "{error}");
    }

    #[test]
    fn raw_data_in_a_non_native_subslot_is_not_accepted() {
        let mut config = vec![9, 0x02, 0, 0, 2, 1, 0, 0x80, 50];
        config.extend(interface(0, 0, 0, 0x01));
        config.extend([8, 0x24, 0x0A, 0x05, 0x03, 0x07, 0x00, 0x00]);
        config.extend(interface(1, 1, 2, 0x02));
        config.extend([
            16, 0x24, 0x01, 0x01, 0x05, 0x01, 0x00, 0x00, 0x00, 0x80, 0x02, 0x03, 0x00, 0x00, 0x00,
            0x00,
        ]);
        // Three-byte subslots cannot hold a 32-bit native DSD word.
        config.extend([6, 0x24, 0x02, 0x01, 0x03, 0x18]);
        config.extend(endpoint(0x01, 0x05, 776));

        assert!(find_native_dsd(&config).is_err());
    }

    #[test]
    fn a_truncated_descriptor_stops_the_walk_instead_of_reading_past_the_end() {
        let mut config = ru7();
        config.truncate(config.len() - 4);

        // The final endpoint is lost, so the last candidate no longer qualifies.
        assert!(find_native_dsd(&config).is_ok());
        assert!(find_native_dsd(&config[..20]).is_err());
    }

    #[test]
    fn a_zero_length_descriptor_does_not_loop_forever() {
        let config = vec![9, 0x02, 0, 0, 1, 1, 0, 0x80, 50, 0, 0, 0];

        assert!(find_native_dsd(&config).is_err());
    }
}
