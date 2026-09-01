use std::fmt;

/// A DSD stream of all-zero audio is a 1010… bit pattern, not zero bytes.
pub const DSD_SILENCE_BYTE: u8 = 0x69;

/// Bit rate of one DSD channel, in bits per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DsdRate(u32);

impl DsdRate {
    pub const fn new(hz: u32) -> Self {
        Self(hz)
    }

    pub const fn hz(self) -> u32 {
        self.0
    }

    /// DoP carries 16 DSD bits per 24-bit PCM frame.
    pub const fn dop_pcm_rate(self) -> u32 {
        self.0 / 16
    }

    /// The `NN` in `DSD64`, relative to whichever CD/DAT base rate divides evenly.
    pub fn multiplier(self) -> Option<u32> {
        for base in [44_100_u32, 48_000] {
            if self.0 % base == 0 {
                return Some(self.0 / base);
            }
        }
        None
    }
}

impl fmt::Display for DsdRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mhz = f64::from(self.0) / 1_000_000.0;
        match self.multiplier() {
            Some(n) => write!(f, "DSD{n} ({mhz:.4} MHz)"),
            None => write!(f, "{mhz:.4} MHz"),
        }
    }
}

/// Order in which a container stores the 8 DSD bits of a byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitOrder {
    MsbFirst,
    LsbFirst,
}

const fn reverse_table() -> [u8; 256] {
    let mut table = [0_u8; 256];
    let mut i = 0;
    while i < 256 {
        table[i] = (i as u8).reverse_bits();
        i += 1;
    }
    table
}

static REVERSED: [u8; 256] = reverse_table();

impl BitOrder {
    /// Rewrite `bytes` in place so the earliest DSD bit sits in bit 7, as DoP requires.
    pub fn normalize_to_msb_first(self, bytes: &mut [u8]) {
        let Self::LsbFirst = self else {
            return;
        };
        for byte in bytes.iter_mut() {
            *byte = REVERSED[*byte as usize];
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DsdFormat {
    pub rate: DsdRate,
    pub channels: u16,
}

impl fmt::Display for DsdFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}, {} ch", self.rate, self.channels)
    }
}

#[cfg(test)]
mod tests {
    use crate::dsd::{BitOrder, DsdRate};

    #[test]
    fn dop_rate_is_a_sixteenth_of_the_dsd_rate() {
        assert_eq!(DsdRate::new(2_822_400).dop_pcm_rate(), 176_400);
        assert_eq!(DsdRate::new(5_644_800).dop_pcm_rate(), 352_800);
        assert_eq!(DsdRate::new(11_289_600).dop_pcm_rate(), 705_600);
    }

    #[test]
    fn multiplier_names_the_dsd_tier() {
        assert_eq!(DsdRate::new(2_822_400).multiplier(), Some(64));
        assert_eq!(DsdRate::new(5_644_800).multiplier(), Some(128));
        assert_eq!(DsdRate::new(3_072_000).multiplier(), Some(64));
    }

    #[test]
    fn lsb_first_bytes_are_reversed_and_msb_first_bytes_are_not() {
        let mut bytes = [0b1000_0001, 0b0000_0010, 0x69];
        BitOrder::MsbFirst.normalize_to_msb_first(&mut bytes);
        assert_eq!(bytes, [0b1000_0001, 0b0000_0010, 0x69]);

        BitOrder::LsbFirst.normalize_to_msb_first(&mut bytes);
        assert_eq!(bytes, [0b1000_0001, 0b0100_0000, 0x96]);
    }
}
