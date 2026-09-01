use crate::dsd::DSD_SILENCE_BYTE;

/// DoP 1.1 marker bytes, alternating in the most significant byte of each frame.
pub const MARKER_A: u8 = 0x05;
pub const MARKER_B: u8 = 0xFA;

/// Payload of a DoP frame carrying DSD silence.
pub const SILENCE_PAYLOAD: u16 = u16::from_be_bytes([DSD_SILENCE_BYTE, DSD_SILENCE_BYTE]);

/// Scale from a 24-bit sample code to the float32 value that encodes it exactly.
pub const FLOAT_SCALE: f32 = 1.0 / 8_388_608.0;

/// Alternating marker generator. A DAC detects DoP by the 0x05/0xFA alternation,
/// so the sequence must never repeat a byte, including across underruns.
#[derive(Debug, Default)]
pub struct Marker {
    high: bool,
}

impl Marker {
    pub const fn new() -> Self {
        Self { high: false }
    }

    pub fn next(&mut self) -> u8 {
        self.high = !self.high;
        if self.high { MARKER_A } else { MARKER_B }
    }
}

/// Build the 24-bit DoP frame word, sign-extended into an i32.
pub const fn word(marker: u8, payload: u16) -> i32 {
    let raw = ((marker as u32) << 16) | payload as u32;
    ((raw << 8) as i32) >> 8
}

/// Recover the marker and DSD payload from a DoP frame word.
#[cfg(test)]
pub const fn split_word(word: i32) -> (u8, u16) {
    let raw = (word as u32) & 0x00FF_FFFF;
    ((raw >> 16) as u8, raw as u16)
}

/// Interleave planar MSB-first DSD into DoP payloads: two DSD bytes per frame per channel.
/// A plane of odd length pairs its final byte with DSD silence.
pub fn pack_planes(planes: &[&[u8]], out: &mut Vec<u16>) -> usize {
    let Some(first) = planes.first() else {
        return 0;
    };
    let frames = first.len().div_ceil(2);
    out.reserve(frames * planes.len());
    for frame in 0..frames {
        for plane in planes {
            let high = plane.get(frame * 2).copied().unwrap_or(DSD_SILENCE_BYTE);
            let low = plane
                .get(frame * 2 + 1)
                .copied()
                .unwrap_or(DSD_SILENCE_BYTE);
            out.push(u16::from_be_bytes([high, low]));
        }
    }
    frames
}

#[cfg(test)]
mod tests {
    use crate::dop::{FLOAT_SCALE, MARKER_A, MARKER_B, Marker, pack_planes, split_word, word};

    fn dop_stream(planes: &[&[u8]]) -> Vec<i32> {
        let mut payloads = Vec::new();
        pack_planes(planes, &mut payloads);
        let mut marker = Marker::new();
        let mut words = Vec::with_capacity(payloads.len());
        for frame in payloads.chunks(planes.len()) {
            let byte = marker.next();
            for payload in frame {
                words.push(word(byte, *payload));
            }
        }
        words
    }

    #[test]
    fn dop_round_trips_every_dsd_byte_of_every_channel() {
        let left: Vec<u8> = (0..=255).collect();
        let right: Vec<u8> = (0..=255).rev().collect();

        let words = dop_stream(&[&left, &right]);

        let mut decoded = [Vec::new(), Vec::new()];
        for (index, frame) in words.chunks(2).enumerate() {
            let expected = if index % 2 == 0 { MARKER_A } else { MARKER_B };
            for (channel, sample) in frame.iter().enumerate() {
                let (marker, payload) = split_word(*sample);
                assert_eq!(marker, expected);
                decoded[channel].extend_from_slice(&payload.to_be_bytes());
            }
        }
        assert_eq!(decoded[0], left);
        assert_eq!(decoded[1], right);
    }

    #[test]
    fn odd_length_plane_is_padded_with_dsd_silence() {
        let words = dop_stream(&[&[0xAB, 0xCD, 0xEF]]);

        assert_eq!(split_word(words[0]).1, 0xABCD);
        assert_eq!(split_word(words[1]).1, 0xEF69);
    }

    #[test]
    fn every_frame_word_survives_the_float32_encoding_unchanged() {
        let markers = [MARKER_A, MARKER_B];
        for marker in markers {
            for payload in 0..=u16::MAX {
                let original = word(marker, payload);
                let encoded = original as f32 * FLOAT_SCALE;
                assert_eq!((encoded / FLOAT_SCALE) as i32, original);
            }
        }
    }
}
