use crate::dsd::DSD_SILENCE_BYTE;

/// Native DSD hands the DAC raw bits: 32 per channel per USB frame, no carrier and no
/// markers. The 32-bit container is the same one alt setting 3 uses for PCM; the DAC tells
/// PCM from DSD by the alternate setting, not by anything in the data.
pub const BYTES_PER_SUBSLOT: usize = 4;

/// Bytes one USB frame carries for `channels` channels.
pub const fn frame_bytes(channels: usize) -> usize {
    channels * BYTES_PER_SUBSLOT
}

/// Interleave planar MSB-first DSD into native subslots: four DSD bytes per channel per
/// frame, earliest byte first, which is the order ALSA calls `DSD_U32_BE`. A plane that does
/// not divide evenly pads its final frame with DSD silence rather than zero, so a short read
/// never drops the DAC out of lock.
pub fn pack_planes(planes: &[&[u8]], out: &mut Vec<u8>) -> usize {
    let Some(first) = planes.first() else {
        return 0;
    };
    let frames = first.len().div_ceil(BYTES_PER_SUBSLOT);
    out.reserve(frames * frame_bytes(planes.len()));
    for frame in 0..frames {
        for plane in planes {
            for byte in 0..BYTES_PER_SUBSLOT {
                let index = frame * BYTES_PER_SUBSLOT + byte;
                out.push(plane.get(index).copied().unwrap_or(DSD_SILENCE_BYTE));
            }
        }
    }
    frames
}

#[cfg(test)]
mod tests {
    use crate::dsd::{DSD_SILENCE_BYTE, DsdRate};
    use crate::native::{BYTES_PER_SUBSLOT, frame_bytes, pack_planes};

    #[test]
    fn native_carries_twice_the_dsd_bits_per_frame_that_dop_does() {
        let rate = DsdRate::new(11_289_600);

        assert_eq!(rate.native_frame_rate(), 352_800);
        assert_eq!(rate.dop_pcm_rate(), 705_600);
    }

    #[test]
    fn every_dsd_byte_of_every_channel_survives_the_round_trip() {
        let left: Vec<u8> = (0..=255).collect();
        let right: Vec<u8> = (0..=255).rev().collect();
        let mut out = Vec::new();

        let frames = pack_planes(&[&left, &right], &mut out);

        assert_eq!(frames, 64);
        assert_eq!(out.len(), frames * frame_bytes(2));
        let mut decoded = [Vec::new(), Vec::new()];
        for frame in out.chunks(frame_bytes(2)) {
            for (channel, subslot) in frame.chunks(BYTES_PER_SUBSLOT).enumerate() {
                decoded[channel].extend_from_slice(subslot);
            }
        }
        assert_eq!(decoded[0], left);
        assert_eq!(decoded[1], right);
    }

    #[test]
    fn earliest_byte_lands_first_in_the_subslot() {
        let mut out = Vec::new();

        pack_planes(
            &[&[0x01, 0x02, 0x03, 0x04], &[0x11, 0x12, 0x13, 0x14]],
            &mut out,
        );

        assert_eq!(out, [0x01, 0x02, 0x03, 0x04, 0x11, 0x12, 0x13, 0x14]);
    }

    #[test]
    fn a_partial_final_frame_is_padded_with_dsd_silence() {
        let mut out = Vec::new();

        let frames = pack_planes(&[&[0xAB, 0xCD]], &mut out);

        assert_eq!(frames, 1);
        assert_eq!(out, [0xAB, 0xCD, DSD_SILENCE_BYTE, DSD_SILENCE_BYTE]);
    }

    #[test]
    fn no_planes_produces_no_frames() {
        let mut out = Vec::new();

        assert_eq!(pack_planes(&[], &mut out), 0);
        assert!(out.is_empty());
    }
}
