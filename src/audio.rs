use std::fmt;

use crate::dsd::DsdFormat;

/// Width every sample reaches the output encoding at. DoP frames are 24 bits by definition,
/// and PCM is left-justified into the same word, so one encoder serves both carriers.
pub const CARRIER_BITS: u32 = 24;

/// Linear PCM as a container stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub rate: u32,
    pub bits: u32,
    pub channels: u16,
}

impl PcmFormat {
    /// Left-justify a container sample in the carrier word.
    ///
    /// Shifting is exact: it scales every code by the same power of two, which is all a wider
    /// container means. Nothing is rounded, dithered, or clipped.
    pub const fn widen(self, sample: i32) -> i32 {
        sample << (CARRIER_BITS - self.bits)
    }
}

impl fmt::Display for PcmFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let khz = f64::from(self.rate) / 1000.0;
        write!(
            f,
            "PCM {khz:.1} kHz {} bit, {} ch",
            self.bits, self.channels
        )
    }
}

/// What a file holds, which decides how it reaches the DAC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    Dsd(DsdFormat),
    Pcm(PcmFormat),
}

impl AudioFormat {
    pub const fn channels(self) -> u16 {
        match self {
            Self::Dsd(format) => format.channels,
            Self::Pcm(format) => format.channels,
        }
    }

    /// The rate the device runs at to carry this: the DoP carrier for DSD, and for PCM the
    /// sample rate itself, which is what locks the DAC to the file rather than to a mixer.
    pub const fn carrier_rate(self) -> u32 {
        match self {
            Self::Dsd(format) => format.rate.dop_pcm_rate(),
            Self::Pcm(format) => format.rate,
        }
    }

    /// Bits of real audio one carrier word holds: DoP fills all 24, and PCM as many as the
    /// container wrote, left-justified in the same word.
    pub const fn carrier_bits(self) -> u32 {
        match self {
            Self::Dsd(_) => CARRIER_BITS,
            Self::Pcm(format) => format.bits,
        }
    }

    /// What the carrier is called where a transport line has room for a word.
    pub const fn carrier_name(self) -> &'static str {
        match self {
            Self::Dsd(_) => "DoP",
            Self::Pcm(_) => "PCM",
        }
    }

    pub const fn dsd(self) -> Option<DsdFormat> {
        match self {
            Self::Dsd(format) => Some(format),
            Self::Pcm(_) => None,
        }
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dsd(format) => format.fmt(f),
            Self::Pcm(format) => format.fmt(f),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::audio::{AudioFormat, PcmFormat};
    use crate::dsd::{DsdFormat, DsdRate};

    #[test]
    fn sixteen_bit_codes_reach_the_carrier_left_justified_and_unchanged() {
        let format = PcmFormat {
            rate: 44_100,
            bits: 16,
            channels: 2,
        };

        assert_eq!(format.widen(i32::from(i16::MAX)), 0x7F_FF00);
        assert_eq!(format.widen(i32::from(i16::MIN)), -0x80_0000);
        assert_eq!(format.widen(0), 0);
    }

    #[test]
    fn twenty_four_bit_codes_reach_the_carrier_untouched() {
        let format = PcmFormat {
            rate: 96_000,
            bits: 24,
            channels: 2,
        };

        assert_eq!(format.widen(0x7F_FFFF), 0x7F_FFFF);
        assert_eq!(format.widen(-0x80_0000), -0x80_0000);
    }

    #[test]
    fn the_carrier_rate_is_the_sample_rate_for_pcm_and_a_sixteenth_for_dsd() {
        let pcm = AudioFormat::Pcm(PcmFormat {
            rate: 96_000,
            bits: 24,
            channels: 2,
        });
        let dsd = AudioFormat::Dsd(DsdFormat {
            rate: DsdRate::new(2_822_400),
            channels: 2,
        });

        assert_eq!(pcm.carrier_rate(), 96_000);
        assert_eq!(dsd.carrier_rate(), 176_400);
    }
}
