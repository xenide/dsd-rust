//! Direct Stream Transfer, the lossless compression a SACD stores its DSD in.
//!
//! ISO/IEC 14496-3 subpart 10. A frame codes one seventy-fifth of a second of every channel
//! and stands on its own: the prediction filters, the probability tables and the arithmetic
//! coder are all read from the frame and reset at its start, so a seek needs no history.
//!
//! The decoder predicts each DSD bit from the 128 that came before it through a filter the
//! frame carries, and codes only whether the prediction was right. Output is the DSD the
//! encoder was given, bit for bit.

use anyhow::{Result, bail, ensure};

use crate::dsd::DSD_SILENCE_BYTE;

pub const MAX_CHANNELS: usize = 6;
/// A frame maps channels onto filters and probability tables, and may give each channel its
/// own of either.
const MAX_ELEMENTS: usize = 2 * MAX_CHANNELS;
/// Taps the prediction filter runs over, as 16 bytes of history looked up a byte at a time.
const TAPS: usize = 16;
const MAX_COEFFICIENTS: usize = 128;
/// The arithmetic coder renormalises whenever its range falls below half of 4096.
const RANGE_FLOOR: u32 = 2048;

/// Coefficients for predicting a filter coefficient from the ones before it, by method.
const FILTER_PREDICTORS: [[i32; 3]; 3] = [[-8, 0, 0], [-16, 8, 0], [-9, -5, 6]];
/// The same, for the probability tables.
const PROBABILITY_PREDICTORS: [[i32; 3]; 3] = [[-8, 0, 0], [-16, 8, 0], [-24, 24, -8]];

/// A bit reader over one frame. Reads past the end come back as zero: the arithmetic coder
/// keeps pulling bits to renormalise after it has consumed the last real one, and the bits
/// it takes then never reach the output.
struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Bits<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn bit(&mut self) -> u32 {
        let byte = self.data.get(self.position >> 3).copied().unwrap_or(0);
        let bit = u32::from(byte >> (7 - (self.position & 7))) & 1;
        self.position += 1;
        bit
    }

    fn take(&mut self, count: u32) -> u32 {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit();
        }
        value
    }

    /// `count` bits read as a two's complement signed field.
    fn take_signed(&mut self, count: u32) -> i32 {
        let value = self.take(count) as i32;
        let sign = 1 << (count - 1);
        (value ^ sign) - sign
    }

    /// Rice code: zeros up to a one, then `k` bits.
    fn golomb(&mut self, k: u32) -> u32 {
        let mut prefix = 0;
        while self.bit() == 0 && !self.exhausted() {
            prefix += 1;
        }
        (prefix << k) + self.take(k)
    }

    /// The same with a sign bit after any non-zero value.
    fn signed_golomb(&mut self, k: u32) -> i32 {
        let value = self.golomb(k) as i32;
        if value != 0 && self.bit() == 1 {
            return -value;
        }
        value
    }

    const fn exhausted(&self) -> bool {
        self.position >= self.data.len() * 8
    }
}

/// The arithmetic decoder of 10.11: a range and a code word, both 12 bits at the start.
struct Coder {
    range: u32,
    code: u32,
}

impl Coder {
    fn new(bits: &mut Bits) -> Self {
        Self {
            range: 4095,
            code: bits.take(12),
        }
    }

    /// Decode one bit against a probability of `p` in 256 that it is zero.
    fn decode(&mut self, bits: &mut Bits, p: u32) -> u32 {
        let scale = (self.range >> 8) | ((self.range >> 7) & 1);
        let split = scale * p;
        let rest = self.range - split;

        let bit = u32::from(self.code < rest);
        if bit == 1 {
            self.range = rest;
        } else {
            self.range = split;
            self.code -= rest;
        }

        if self.range < RANGE_FLOOR {
            let shift = 11 - (31 - self.range.max(1).leading_zeros());
            self.range <<= shift;
            self.code = (self.code << shift) | bits.take(shift);
        }
        bit
    }
}

/// One frame's filter coefficients, or its probability tables: the two are read the same way
/// and differ only in how wide their fields are and whether they are signed.
struct Table {
    elements: usize,
    length: [usize; MAX_ELEMENTS],
    coefficients: Box<[[i32; MAX_COEFFICIENTS]; MAX_ELEMENTS]>,
}

impl Table {
    fn new() -> Self {
        Self {
            elements: 1,
            length: [0; MAX_ELEMENTS],
            coefficients: Box::new([[0; MAX_COEFFICIENTS]; MAX_ELEMENTS]),
        }
    }

    /// Which element each channel uses. One bit says every channel shares element 0;
    /// otherwise each channel names an element, and naming the next one in line creates it.
    fn read_map(&mut self, bits: &mut Bits, channels: usize, map: &mut [usize]) -> Result<()> {
        self.elements = 1;
        map[0] = 0;
        if bits.bit() == 1 {
            map[..channels].fill(0);
            return Ok(());
        }
        for slot in map.iter_mut().take(channels).skip(1) {
            let width = 32 - (self.elements as u32).leading_zeros();
            let element = bits.take(width) as usize;
            ensure!(
                element <= self.elements,
                "DST element {element} is out of order"
            );
            if element == self.elements {
                self.elements += 1;
                ensure!(self.elements < MAX_ELEMENTS, "too many DST elements");
            }
            *slot = element;
        }
        Ok(())
    }

    /// Coefficients for every element, either written out or predicted from their neighbours
    /// and corrected by a Rice-coded residual.
    fn read(&mut self, bits: &mut Bits, shape: Shape) -> Result<()> {
        for element in 0..self.elements {
            let length = bits.take(shape.length_bits) as usize + 1;
            ensure!(
                length <= MAX_COEFFICIENTS,
                "DST element {element} declares {length} coefficients"
            );
            self.length[element] = length;
            let coefficients = &mut self.coefficients[element];
            if bits.bit() == 0 {
                for slot in coefficients.iter_mut().take(length) {
                    *slot = shape.read_one(bits);
                }
                continue;
            }

            let method = bits.take(2) as usize;
            ensure!(method < 3, "reserved DST coefficient prediction method");
            for slot in coefficients.iter_mut().take(method + 1) {
                *slot = shape.read_one(bits);
            }
            let residual_bits = bits.take(3);
            for index in method + 1..length {
                let mut predicted = 0_i32;
                for tap in 0..=method {
                    predicted = predicted.wrapping_add(
                        shape.predictors[method][tap].wrapping_mul(coefficients[index - tap - 1]),
                    );
                }
                let mut value = bits.signed_golomb(residual_bits);
                if predicted >= 0 {
                    value -= (predicted + 4) / 8;
                } else {
                    value += (-predicted + 3) / 8;
                }
                if !shape.signed {
                    let limit = shape.offset + (1 << shape.coefficient_bits);
                    ensure!(
                        (shape.offset..limit).contains(&value),
                        "DST probability {value} is outside 1..={limit}"
                    );
                }
                coefficients[index] = value;
            }
        }
        Ok(())
    }
}

/// How one table's coefficients are written down.
#[derive(Clone, Copy)]
struct Shape {
    length_bits: u32,
    coefficient_bits: u32,
    signed: bool,
    offset: i32,
    predictors: [[i32; 3]; 3],
}

const FILTER_SHAPE: Shape = Shape {
    length_bits: 7,
    coefficient_bits: 9,
    signed: true,
    offset: 0,
    predictors: FILTER_PREDICTORS,
};

const PROBABILITY_SHAPE: Shape = Shape {
    length_bits: 6,
    coefficient_bits: 7,
    signed: false,
    offset: 1,
    predictors: PROBABILITY_PREDICTORS,
};

impl Shape {
    fn read_one(self, bits: &mut Bits) -> i32 {
        if self.signed {
            bits.take_signed(self.coefficient_bits) + self.offset
        } else {
            bits.take(self.coefficient_bits) as i32 + self.offset
        }
    }
}

/// A DST decoder, reused across frames so its tables are allocated once.
pub struct Decoder {
    channels: usize,
    /// DSD bits one frame carries per channel.
    samples_per_frame: usize,
    filters: Table,
    probabilities: Table,
    /// The filter collapsed into a lookup: for each element, tap and byte of history, what
    /// those eight bits contribute to the prediction.
    taps: Box<[[[i16; 256]; TAPS]; MAX_ELEMENTS]>,
    /// The last 128 bits each channel produced, newest in the low bit.
    history: [u128; MAX_CHANNELS],
    filter_of: [usize; MAX_CHANNELS],
    probability_of: [usize; MAX_CHANNELS],
    half_probability: [bool; MAX_CHANNELS],
}

impl Decoder {
    pub fn new(channels: usize, samples_per_frame: usize) -> Result<Self> {
        ensure!(
            (1..=MAX_CHANNELS).contains(&channels),
            "DST carries at most {MAX_CHANNELS} channels, not {channels}"
        );
        ensure!(
            samples_per_frame % 8 == 0,
            "a DST frame of {samples_per_frame} bits is not a whole number of bytes"
        );
        Ok(Self {
            channels,
            samples_per_frame,
            filters: Table::new(),
            probabilities: Table::new(),
            taps: Box::new([[[0; 256]; TAPS]; MAX_ELEMENTS]),
            history: [0; MAX_CHANNELS],
            filter_of: [0; MAX_CHANNELS],
            probability_of: [0; MAX_CHANNELS],
            half_probability: [false; MAX_CHANNELS],
        })
    }

    pub const fn frame_bytes(&self) -> usize {
        self.samples_per_frame / 8
    }

    /// Decode one frame into the head of each plane, MSB first, as DoP and DSDIFF want it.
    pub fn decode(&mut self, frame: &[u8], planes: &mut [Box<[u8]>]) -> Result<()> {
        ensure!(frame.len() > 1, "a DST frame of {} bytes", frame.len());
        let bytes = self.frame_bytes();
        for plane in planes.iter_mut().take(self.channels) {
            plane[..bytes].fill(0);
        }

        let mut bits = Bits::new(frame);
        if bits.bit() == 0 {
            return self.copy_uncoded(frame, planes);
        }
        self.read_header(&mut bits)?;
        self.build_taps()?;
        self.run(&mut bits, planes);
        Ok(())
    }

    /// A frame the encoder could not compress carries plain DSD, one byte per channel in
    /// turn. A short one is padded with DSD silence rather than zero, which would be a click.
    fn copy_uncoded(&self, frame: &[u8], planes: &mut [Box<[u8]>]) -> Result<()> {
        ensure!(
            frame[0] & 0x3F == 0,
            "an uncoded DST frame with a non-zero header"
        );
        let bytes = self.frame_bytes();
        let body = &frame[1..];
        for (channel, plane) in planes.iter_mut().enumerate().take(self.channels) {
            for index in 0..bytes {
                plane[index] = body
                    .get(index * self.channels + channel)
                    .copied()
                    .unwrap_or(DSD_SILENCE_BYTE);
            }
        }
        Ok(())
    }

    /// Everything ahead of the arithmetic-coded data: segmentation, the channel-to-element
    /// maps, the filters and the probability tables.
    fn read_header(&mut self, bits: &mut Bits) -> Result<()> {
        // Segmentation (10.4 to 10.6). Every disc in the wild codes each frame as one
        // segment, and a decoder that pretended otherwise would be untested code.
        for what in ["", " for all channels", " to the end of the channel"] {
            if bits.bit() == 0 {
                bail!("this SACD splits DST frames into segments{what}, which is not supported");
            }
        }

        let same_map = bits.bit() == 1;
        let mut filter_of = [0; MAX_CHANNELS];
        self.filters.read_map(bits, self.channels, &mut filter_of)?;
        self.filter_of = filter_of;
        if same_map {
            self.probabilities.elements = self.filters.elements;
            self.probability_of = filter_of;
        } else {
            let mut probability_of = [0; MAX_CHANNELS];
            self.probabilities
                .read_map(bits, self.channels, &mut probability_of)?;
            self.probability_of = probability_of;
        }

        for channel in 0..self.channels {
            self.half_probability[channel] = bits.bit() == 1;
        }
        self.filters.read(bits, FILTER_SHAPE)?;
        self.probabilities.read(bits, PROBABILITY_SHAPE)?;
        ensure!(bits.bit() == 0, "DST frame reserves a bit it must not set");
        Ok(())
    }

    /// Collapse each filter into what every possible byte of history contributes, so the
    /// prediction costs sixteen lookups per bit instead of a hundred and twenty-eight
    /// multiplies. A history bit of 1 adds its coefficient and a 0 subtracts it.
    fn build_taps(&mut self) -> Result<()> {
        for element in 0..self.filters.elements {
            let length = self.filters.length[element];
            let coefficients = &self.filters.coefficients[element];
            for tap in 0..TAPS {
                let used = length.saturating_sub(tap * 8).min(8);
                for byte in 0_usize..256 {
                    let mut total = 0_i32;
                    for index in 0..used {
                        let sign = ((byte >> index) & 1) as i32 * 2 - 1;
                        total += sign * coefficients[tap * 8 + index];
                    }
                    let Ok(total) = i16::try_from(total) else {
                        bail!("DST filter coefficients sum past what a prediction can hold");
                    };
                    self.taps[element][tap][byte] = total;
                }
            }
        }
        Ok(())
    }

    /// Decode every bit of the frame. One pass over the arithmetic-coded data, predicting
    /// each channel's next DSD bit from its own history and correcting it by the coded
    /// residual.
    fn run(&mut self, bits: &mut Bits, planes: &mut [Box<[u8]>]) {
        self.history = [0xAAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA_AAAA; MAX_CHANNELS];
        let mut coder = Coder::new(bits);
        // The first coded bit says whether the frame was coded at all, which the flag at the
        // top of the frame has already answered. It still has to come out of the coder.
        coder.decode(bits, half_byte_probability(self.filters.coefficients[0][0]));

        let channels = self.channels;
        for sample in 0..self.samples_per_frame {
            for (channel, plane) in planes.iter_mut().enumerate().take(channels) {
                let element = self.filter_of[channel];
                let taps = &self.taps[element];
                let history = self.history[channel].to_le_bytes();
                let mut prediction = 0_i32;
                for (tap, byte) in history.iter().enumerate() {
                    prediction += i32::from(taps[tap][*byte as usize]);
                }
                let prediction = prediction as i16;

                let probability = if self.half_probability[channel]
                    && sample < self.filters.length[element]
                {
                    128
                } else {
                    let table = self.probability_of[channel];
                    let index = (prediction.unsigned_abs() >> 3) as usize;
                    self.probabilities.coefficients[table]
                        [index.min(self.probabilities.length[table] - 1)] as u32
                };

                let residual = coder.decode(bits, probability);
                let value = ((prediction >> 15) as u32 ^ residual) & 1;
                plane[sample >> 3] |= (value as u8) << (7 - (sample & 7));
                self.history[channel] = (self.history[channel] << 1) | u128::from(value);
            }
        }
    }
}

/// The probability the frame's opening bit is coded against: the first filter coefficient's
/// low seven bits, reversed. It exists so that bit costs about as much as any other.
fn half_byte_probability(coefficient: i32) -> u32 {
    u32::from(((coefficient & 127) as u8).reverse_bits() >> 1) + 1
}

#[cfg(test)]
mod tests {
    use crate::dsd::DSD_SILENCE_BYTE;
    use crate::reader::sacd::dst::{Bits, Decoder, half_byte_probability};

    fn planes(count: usize, bytes: usize) -> Vec<Box<[u8]>> {
        vec![vec![0_u8; bytes].into(); count]
    }

    #[test]
    fn bits_come_out_most_significant_first_and_run_out_as_zero() {
        let mut bits = Bits::new(&[0b1011_0010, 0xFF]);

        assert_eq!(bits.take(4), 0b1011);
        assert_eq!(bits.take(4), 0b0010);
        assert_eq!(bits.take(8), 0xFF);
        assert_eq!(bits.take(8), 0);
    }

    #[test]
    fn signed_fields_sign_extend_from_their_own_width() {
        let mut bits = Bits::new(&[0b1000_0000, 0b0111_1111]);

        assert_eq!(bits.take_signed(9), -256);
        assert_eq!(bits.take_signed(7), -1);
    }

    #[test]
    fn a_rice_code_is_its_zeros_then_its_low_bits() {
        // 0001 selects a prefix of 3, then two low bits 10: (3 << 2) + 2.
        let mut bits = Bits::new(&[0b0001_1000]);

        assert_eq!(bits.golomb(2), 14);
    }

    #[test]
    fn an_uncoded_frame_hands_back_the_dsd_it_carries_interleaved_by_byte() {
        let mut decoder = Decoder::new(2, 16).expect("two channels");
        let mut planes = planes(2, 2);
        // A zero flag byte, then one byte per channel in turn.
        let frame = [0x00, 0x12, 0xAB, 0x34, 0xCD];

        decoder.decode(&frame, &mut planes).expect("decodes");

        assert_eq!(&planes[0][..2], [0x12, 0x34]);
        assert_eq!(&planes[1][..2], [0xAB, 0xCD]);
    }

    #[test]
    fn an_uncoded_frame_that_runs_short_is_padded_with_dsd_silence() {
        let mut decoder = Decoder::new(2, 16).expect("two channels");
        let mut planes = planes(2, 2);

        decoder
            .decode(&[0x00, 0x12, 0xAB], &mut planes)
            .expect("decodes");

        assert_eq!(&planes[0][..2], [0x12, DSD_SILENCE_BYTE]);
        assert_eq!(&planes[1][..2], [0xAB, DSD_SILENCE_BYTE]);
    }

    #[test]
    fn the_opening_probability_reverses_the_low_seven_bits_of_the_first_coefficient() {
        assert_eq!(half_byte_probability(0), 1);
        assert_eq!(half_byte_probability(1), 65);
        assert_eq!(half_byte_probability(127), 128);
        // A negative coefficient keeps its low seven bits, as two's complement leaves them.
        assert_eq!(half_byte_probability(-1), 128);
    }

    #[test]
    fn a_frame_too_short_to_hold_a_header_is_refused() {
        let mut decoder = Decoder::new(2, 16).expect("two channels");

        let error = decoder
            .decode(&[0x80], &mut planes(2, 2))
            .expect_err("refused");

        assert!(error.to_string().contains("DST frame"), "{error}");
    }
}
