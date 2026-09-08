use anyhow::{Result, bail};
use coreaudio_sys::{
    AudioStreamBasicDescription, kAudioFormatFlagIsAlignedHigh, kAudioFormatFlagIsBigEndian,
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonMixable, kAudioFormatFlagIsSignedInteger,
    kAudioFormatLinearPCM,
};

use crate::dop::FLOAT_SCALE;
use crate::native::BYTES_PER_SUBSLOT;

/// How the DSD bits reach the DAC through Core Audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Carrier {
    /// DoP 1.1: an alternating marker byte and 16 DSD bits per channel in each frame.
    Dop,
    /// Native DSD: 32 raw DSD bits per channel in each frame, no marker and no carrier.
    /// A driver that owns the DAC's `RAW_DATA` alternate setting publishes it as big-endian
    /// non-mixable 32-bit integer PCM, which is what ALSA calls `DSD_U32_BE`.
    NativeDsd,
}

impl Carrier {
    /// DSD byte pairs one device frame carries per channel. The queue between the reader and
    /// the render callback holds those pairs whichever carrier takes them, so this is the
    /// only place the two differ in how much of it one frame consumes.
    pub const fn payloads_per_frame(self) -> usize {
        match self {
            Self::Dop => 1,
            Self::NativeDsd => 2,
        }
    }

    /// What the transport display calls this carrier.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Dop => "DoP",
            Self::NativeDsd => "native DSD",
        }
    }

    /// Bits of container one frame gives each channel.
    pub const fn bits(self) -> u8 {
        match self {
            Self::Dop => 24,
            Self::NativeDsd => 32,
        }
    }
}

/// Whether a stream format is the one a driver publishes for native DSD.
///
/// Nothing in the format says "DSD": Core Audio has no such format ID, so native goes out as
/// integer PCM of the same width and rate as ordinary 32-bit PCM. Only the big-endian flag
/// separates the two, and the driver reads exactly that flag to choose an alternate setting.
/// Getting it wrong either way is audible, so all of the flags have to agree before a format
/// counts as native.
pub fn is_native_dsd(format: &AudioStreamBasicDescription) -> bool {
    const REQUIRED: u32 = kAudioFormatFlagIsSignedInteger
        | kAudioFormatFlagIsBigEndian
        | kAudioFormatFlagIsNonMixable;
    let channels = format.mChannelsPerFrame.max(1);
    format.mFormatID == kAudioFormatLinearPCM
        && format.mFormatFlags & REQUIRED == REQUIRED
        && format.mBitsPerChannel == BYTES_PER_SUBSLOT as u32 * 8
        && format.mBytesPerFrame / channels == BYTES_PER_SUBSLOT as u32
}

/// How a 24-bit DoP word is laid out in one sample of the device's stream format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Float32,
    Signed {
        bytes: u8,
        shift: u8,
        big_endian: bool,
    },
}

impl Encoding {
    pub fn from_format(format: &AudioStreamBasicDescription) -> Result<Self> {
        if format.mFormatID != kAudioFormatLinearPCM {
            bail!("stream is not linear PCM");
        }
        let channels = format.mChannelsPerFrame.max(1);
        let bytes = u8::try_from(format.mBytesPerFrame / channels)?;
        let bits = format.mBitsPerChannel;
        let flags = format.mFormatFlags;
        let big_endian = flags & kAudioFormatFlagIsBigEndian != 0;

        if flags & kAudioFormatFlagIsFloat != 0 {
            if bits != 32 || bytes != 4 || big_endian {
                bail!("unsupported float stream format: {bits} bits in {bytes} bytes");
            }
            return Ok(Self::Float32);
        }
        if flags & kAudioFormatFlagIsSignedInteger == 0 {
            bail!("unsigned integer stream formats cannot carry DoP");
        }
        if bits < 24 {
            bail!("DoP needs at least 24 bits per sample, the stream offers {bits}");
        }

        let aligned_high = flags & kAudioFormatFlagIsAlignedHigh != 0;
        let shift = match (bytes, bits) {
            (3, 24) => 0,
            (4, 24) if aligned_high => 8,
            (4, 24) => 0,
            (4, 32) => 8,
            _ => bail!("unsupported integer stream format: {bits} bits in {bytes} bytes"),
        };
        Ok(Self::Signed {
            bytes,
            shift,
            big_endian,
        })
    }

    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Float32 => 4,
            Self::Signed { bytes, .. } => bytes as usize,
        }
    }

    /// True when the samples reach the device exactly as written, with no conversion.
    pub const fn is_integer(self) -> bool {
        match self {
            Self::Float32 => false,
            Self::Signed { .. } => true,
        }
    }

    /// Write one DoP word into `out`, which must be [`Encoding::bytes_per_sample`] long.
    pub fn write(self, word: i32, out: &mut [u8]) {
        match self {
            Self::Float32 => out.copy_from_slice(&(word as f32 * FLOAT_SCALE).to_le_bytes()),
            Self::Signed {
                bytes,
                shift,
                big_endian,
            } => {
                let value = (word << shift) as u32;
                let ordered = if big_endian {
                    value.to_be_bytes()
                } else {
                    value.to_le_bytes()
                };
                if bytes == 3 {
                    let start = if big_endian { 1 } else { 0 };
                    out.copy_from_slice(&ordered[start..start + 3]);
                } else {
                    out.copy_from_slice(&ordered);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use coreaudio_sys::{
        AudioStreamBasicDescription, kAudioFormatFlagIsAlignedHigh, kAudioFormatFlagIsBigEndian,
        kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked, kAudioFormatFlagIsSignedInteger,
        kAudioFormatLinearPCM,
    };

    use crate::dop::{MARKER_A, MARKER_B, word};
    use crate::output::encoding::Encoding;

    fn format(flags: u32, bits: u32, bytes_per_frame: u32) -> AudioStreamBasicDescription {
        AudioStreamBasicDescription {
            mSampleRate: 176_400.0,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: flags,
            mBytesPerPacket: bytes_per_frame,
            mFramesPerPacket: 1,
            mBytesPerFrame: bytes_per_frame,
            mChannelsPerFrame: 2,
            mBitsPerChannel: bits,
            mReserved: 0,
        }
    }

    #[test]
    fn packed_24_bit_little_endian_carries_the_marker_in_the_top_byte() {
        let encoding = Encoding::from_format(&format(
            kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked,
            24,
            6,
        ))
        .expect("supported");
        let mut out = [0_u8; 3];

        encoding.write(word(MARKER_B, 0xABCD), &mut out);

        assert_eq!(
            encoding,
            Encoding::Signed {
                bytes: 3,
                shift: 0,
                big_endian: false
            }
        );
        assert_eq!(out, [0xCD, 0xAB, 0xFA]);
    }

    #[test]
    fn packed_24_bit_big_endian_keeps_the_marker_first() {
        let flags = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsBigEndian;
        let encoding = Encoding::from_format(&format(flags, 24, 6)).expect("supported");
        let mut out = [0_u8; 3];

        encoding.write(word(MARKER_A, 0xABCD), &mut out);

        assert_eq!(out, [0x05, 0xAB, 0xCD]);
    }

    #[test]
    fn high_aligned_24_in_32_leaves_the_low_byte_empty() {
        let flags = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsAlignedHigh;
        let encoding = Encoding::from_format(&format(flags, 24, 8)).expect("supported");
        let mut out = [0_u8; 4];

        encoding.write(word(MARKER_A, 0xABCD), &mut out);

        assert_eq!(out, [0x00, 0xCD, 0xAB, 0x05]);
    }

    #[test]
    fn low_aligned_24_in_32_sign_extends_the_negative_marker() {
        let encoding = Encoding::from_format(&format(kAudioFormatFlagIsSignedInteger, 24, 8))
            .expect("supported");
        let mut out = [0_u8; 4];

        encoding.write(word(MARKER_B, 0x0000), &mut out);

        assert_eq!(i32::from_le_bytes(out), word(MARKER_B, 0x0000));
    }

    #[test]
    fn float32_encodes_the_word_exactly() {
        let encoding =
            Encoding::from_format(&format(kAudioFormatFlagIsFloat, 32, 8)).expect("supported");
        let mut out = [0_u8; 4];

        encoding.write(word(MARKER_A, 0x1234), &mut out);

        let decoded = f32::from_le_bytes(out) * 8_388_608.0;
        assert_eq!(decoded as i32, word(MARKER_A, 0x1234));
        assert!(!encoding.is_integer());
    }

    #[test]
    fn sixteen_bit_streams_are_rejected() {
        let error = Encoding::from_format(&format(kAudioFormatFlagIsSignedInteger, 16, 4))
            .expect_err("rejected");

        assert!(error.to_string().contains("at least 24 bits"), "{error}");
    }
}
