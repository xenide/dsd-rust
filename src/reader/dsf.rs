use std::io::Read;

use anyhow::{Result, bail, ensure};

use crate::dsd::{BitOrder, DsdFormat, DsdRate};
use crate::reader::{DsdSource, read_available};

const DSD_CHUNK_LEN: usize = 28;
const FMT_CHUNK_LEN: usize = 52;
const DATA_HEADER_LEN: usize = 12;
const FORMAT_ID_RAW_DSD: u32 = 0;

/// Reader for Sony's DSF container: fixed-size per-channel blocks, one channel after another.
pub struct DsfReader<R> {
    inner: R,
    format: DsdFormat,
    bit_order: BitOrder,
    block_bytes: usize,
    audio_bytes_per_channel: u64,
    emitted: u64,
    scratch: Vec<u8>,
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("4 bytes in range"),
    )
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("8 bytes in range"),
    )
}

impl<R: Read + Send + 'static> DsfReader<R> {
    pub fn new(mut inner: R) -> Result<Self> {
        let mut header = [0_u8; DSD_CHUNK_LEN];
        inner.read_exact(&mut header)?;
        ensure!(&header[0..4] == b"DSD ", "missing DSD chunk");
        ensure!(
            u64_at(&header, 4) == DSD_CHUNK_LEN as u64,
            "bad DSD chunk size"
        );

        let mut fmt = [0_u8; FMT_CHUNK_LEN];
        inner.read_exact(&mut fmt)?;
        ensure!(&fmt[0..4] == b"fmt ", "missing fmt chunk");
        ensure!(
            u64_at(&fmt, 4) == FMT_CHUNK_LEN as u64,
            "bad fmt chunk size"
        );
        ensure!(u32_at(&fmt, 12) == 1, "unsupported DSF format version");

        let format_id = u32_at(&fmt, 16);
        ensure!(
            format_id == FORMAT_ID_RAW_DSD,
            "unsupported DSF format id {format_id}"
        );

        let channels = u32_at(&fmt, 24);
        ensure!(
            (1..=6).contains(&channels),
            "unsupported channel count {channels}"
        );

        let bit_order = match u32_at(&fmt, 32) {
            1 => BitOrder::LsbFirst,
            8 => BitOrder::MsbFirst,
            other => bail!("unsupported bits-per-sample field {other}"),
        };

        let sample_count = u64_at(&fmt, 36);
        let block_bytes = u32_at(&fmt, 44) as usize;
        ensure!(
            block_bytes > 0 && block_bytes % 2 == 0,
            "bad block size {block_bytes}"
        );

        let mut data = [0_u8; DATA_HEADER_LEN];
        inner.read_exact(&mut data)?;
        ensure!(&data[0..4] == b"data", "missing data chunk");
        let data_bytes = u64_at(&data, 4).saturating_sub(DATA_HEADER_LEN as u64);

        let audio_bytes_per_channel = (sample_count / 8).min(data_bytes / u64::from(channels));
        ensure!(audio_bytes_per_channel > 0, "no audio in data chunk");

        let channels = u16::try_from(channels).expect("channel count fits u16");
        Ok(Self {
            inner,
            format: DsdFormat {
                rate: DsdRate::new(u32_at(&fmt, 28)),
                channels,
            },
            bit_order,
            block_bytes,
            audio_bytes_per_channel,
            emitted: 0,
            scratch: vec![0; block_bytes * channels as usize],
        })
    }
}

impl<R: Read + Send + 'static> DsdSource for DsfReader<R> {
    fn container(&self) -> &'static str {
        "DSF"
    }

    fn format(&self) -> DsdFormat {
        self.format
    }

    fn total_bytes_per_channel(&self) -> u64 {
        self.audio_bytes_per_channel
    }

    fn chunk_bytes(&self) -> usize {
        self.block_bytes
    }

    fn read(&mut self, planes: &mut [Box<[u8]>]) -> Result<usize> {
        let remaining = self.audio_bytes_per_channel - self.emitted;
        if remaining == 0 {
            return Ok(0);
        }

        let channels = self.format.channels as usize;
        let filled = read_available(&mut self.inner, &mut self.scratch)?;
        let last_channel_start = (channels - 1) * self.block_bytes;
        let available = filled
            .saturating_sub(last_channel_start)
            .min(self.block_bytes);
        let count = available.min(remaining as usize);
        if count == 0 {
            return Ok(0);
        }

        for (channel, plane) in planes.iter_mut().enumerate().take(channels) {
            let start = channel * self.block_bytes;
            plane[..count].copy_from_slice(&self.scratch[start..start + count]);
            self.bit_order.normalize_to_msb_first(&mut plane[..count]);
        }
        self.emitted += count as u64;
        Ok(count)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Cursor;

    use crate::dsd::{DSD_SILENCE_BYTE, DsdRate};
    use crate::reader::DsdSource;
    use crate::reader::dsf::DsfReader;

    pub(crate) const BLOCK: usize = 8;

    /// Build a two-channel DSF whose data chunk holds `blocks` block pairs.
    pub(crate) fn dsf_file(
        bits_per_sample: u32,
        samples_per_channel: u64,
        blocks: usize,
    ) -> Vec<u8> {
        let data_bytes = BLOCK * 2 * blocks;
        let mut file = Vec::new();
        file.extend_from_slice(b"DSD ");
        file.extend_from_slice(&28_u64.to_le_bytes());
        file.extend_from_slice(&0_u64.to_le_bytes());
        file.extend_from_slice(&0_u64.to_le_bytes());

        file.extend_from_slice(b"fmt ");
        file.extend_from_slice(&52_u64.to_le_bytes());
        for field in [1_u32, 0, 2, 2, 2_822_400, bits_per_sample] {
            file.extend_from_slice(&field.to_le_bytes());
        }
        file.extend_from_slice(&samples_per_channel.to_le_bytes());
        file.extend_from_slice(&(BLOCK as u32).to_le_bytes());
        file.extend_from_slice(&0_u32.to_le_bytes());

        file.extend_from_slice(b"data");
        file.extend_from_slice(&((data_bytes + 12) as u64).to_le_bytes());
        for block in 0..blocks {
            for channel in 0..2_u8 {
                for byte in 0..BLOCK {
                    let index = (block * BLOCK + byte) as u8;
                    file.push(if index as usize >= 12 {
                        DSD_SILENCE_BYTE
                    } else {
                        index | channel << 6
                    });
                }
            }
        }
        file
    }

    fn drain(source: &mut dyn DsdSource) -> Vec<Vec<u8>> {
        let channels = source.format().channels as usize;
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; source.chunk_bytes()].into(); channels];
        let mut out = vec![Vec::new(); channels];
        loop {
            let count = source.read(&mut planes).expect("read succeeds");
            if count == 0 {
                return out;
            }
            for (channel, plane) in planes.iter().enumerate() {
                out[channel].extend_from_slice(&plane[..count]);
            }
        }
    }

    #[test]
    fn header_fields_describe_the_stream() {
        let reader = DsfReader::new(Cursor::new(dsf_file(1, 96, 2))).expect("parses");

        assert_eq!(reader.format().rate, DsdRate::new(2_822_400));
        assert_eq!(reader.format().channels, 2);
        assert_eq!(reader.total_bytes_per_channel(), 12);
        assert_eq!(reader.chunk_bytes(), BLOCK);
    }

    #[test]
    fn msb_first_data_is_deinterleaved_and_padding_is_trimmed() {
        let mut reader = DsfReader::new(Cursor::new(dsf_file(8, 96, 2))).expect("parses");

        let planes = drain(&mut reader);

        assert_eq!(planes[0], (0..12_u8).collect::<Vec<_>>());
        assert_eq!(
            planes[1],
            (0..12_u8).map(|byte| byte | 0x40).collect::<Vec<_>>()
        );
    }

    #[test]
    fn lsb_first_data_is_flipped_to_msb_first() {
        let mut reader = DsfReader::new(Cursor::new(dsf_file(1, 96, 2))).expect("parses");

        let planes = drain(&mut reader);

        assert_eq!(
            planes[0],
            (0..12_u8).map(u8::reverse_bits).collect::<Vec<_>>()
        );
    }

    #[test]
    fn truncated_data_stops_at_the_last_complete_block_pair() {
        let mut file = dsf_file(8, 160, 2);
        file.truncate(file.len() - BLOCK - 3);

        let planes = drain(&mut DsfReader::new(Cursor::new(file)).expect("parses"));

        assert_eq!(planes[0].len(), BLOCK);
        assert_eq!(planes[1].len(), BLOCK);
    }

    #[test]
    fn a_bad_magic_is_rejected() {
        let mut file = dsf_file(8, 96, 1);
        file[0] = b'X';

        let error = DsfReader::new(Cursor::new(file))
            .map(|_| ())
            .expect_err("rejects");

        assert!(error.to_string().contains("missing DSD chunk"), "{error}");
    }
}
