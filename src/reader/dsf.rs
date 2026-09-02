use std::io::{Read, Seek, SeekFrom};

use anyhow::{Result, bail, ensure};

use crate::dsd::{BitOrder, DsdFormat, DsdRate};
use crate::reader::tags::{TrackTags, read_id3v2};
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
    /// Where the audio starts, so a seek can address the blocks from it.
    data_start: u64,
    emitted: u64,
    scratch: Vec<u8>,
    tags: TrackTags,
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

impl<R: Read + Seek + Send + 'static> DsfReader<R> {
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

        let data_start = inner.stream_position()?;
        // The DSD chunk points at an ID3v2 tag past the audio, or at nothing when the file
        // carries none. Reading it moves the cursor, so the audio position is set back after.
        let metadata_start = u64_at(&header, 20);
        let tags = match metadata_start {
            0 => TrackTags::default(),
            offset => {
                let tags = read_id3v2(&mut inner, offset)?;
                inner.seek(SeekFrom::Start(data_start))?;
                tags
            }
        };

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
            data_start,
            emitted: 0,
            scratch: vec![0; block_bytes * channels as usize],
            tags,
        })
    }
}

impl<R: Read + Seek + Send + 'static> DsdSource for DsfReader<R> {
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

    fn tags(&self) -> &TrackTags {
        &self.tags
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

    /// DSF stores whole blocks, one channel after another, so a position is only addressable
    /// at a block boundary.
    fn seek(&mut self, bytes_per_channel: u64) -> Result<u64> {
        let block_bytes = self.block_bytes as u64;
        let block = bytes_per_channel.min(self.audio_bytes_per_channel) / block_bytes;
        let channels = u64::from(self.format.channels);
        self.inner.seek(SeekFrom::Start(
            self.data_start + block * block_bytes * channels,
        ))?;
        self.emitted = block * block_bytes;
        Ok(self.emitted)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Cursor;

    use crate::dsd::{DSD_SILENCE_BYTE, DsdRate};
    use crate::reader::DsdSource;
    use crate::reader::dsf::DsfReader;
    use crate::reader::tags::TrackTags;

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

    /// Append an ID3v2.3 tag holding one title frame, and point the DSD chunk at it.
    pub(crate) fn dsf_file_with_tag(title: &str) -> Vec<u8> {
        let mut file = dsf_file(8, 96, 2);
        let mut frames = Vec::new();
        frames.extend_from_slice(b"TIT2");
        frames.extend_from_slice(&((title.len() + 1) as u32).to_be_bytes());
        frames.extend_from_slice(&[0, 0, 0]);
        frames.extend_from_slice(title.as_bytes());

        let start = file.len() as u64;
        file[20..28].copy_from_slice(&start.to_le_bytes());
        file.extend_from_slice(b"ID3");
        file.extend_from_slice(&[3, 0, 0]);
        for shift in [21, 14, 7, 0] {
            file.push(((frames.len() as u32 >> shift) & 0x7F) as u8);
        }
        file.extend_from_slice(&frames);
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
    fn a_seek_rounds_back_to_a_block_boundary_and_reads_on_from_there() {
        let mut reader = DsfReader::new(Cursor::new(dsf_file(8, 96, 2))).expect("parses");
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; BLOCK].into(); 2];

        let reached = reader.seek(BLOCK as u64 + 3).expect("seeks");

        assert_eq!(reached, BLOCK as u64);
        let count = reader.read(&mut planes).expect("reads");
        assert_eq!(count, 4);
        assert_eq!(planes[0][..4], [8, 9, 10, 11]);
    }

    #[test]
    fn a_seek_back_inside_the_first_block_starts_the_file_again() {
        let mut reader = DsfReader::new(Cursor::new(dsf_file(8, 96, 2))).expect("parses");
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; BLOCK].into(); 2];
        reader.read(&mut planes).expect("reads");

        let reached = reader.seek(3).expect("seeks");

        assert_eq!(reached, 0);
        assert_eq!(reader.read(&mut planes).expect("reads"), BLOCK);
        assert_eq!(planes[0][..BLOCK], [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn a_seek_past_the_end_stops_at_the_last_block_of_audio() {
        let mut reader = DsfReader::new(Cursor::new(dsf_file(8, 96, 2))).expect("parses");

        let reached = reader.seek(u64::MAX).expect("seeks");

        assert_eq!(reached, BLOCK as u64);
    }

    #[test]
    fn a_tag_past_the_audio_is_read_without_moving_the_read_position() {
        let mut reader = DsfReader::new(Cursor::new(dsf_file_with_tag("So What"))).expect("parses");

        assert_eq!(reader.tags().title.as_deref(), Some("So What"));
        let planes = drain(&mut reader);
        assert_eq!(planes[0], (0..12_u8).collect::<Vec<_>>());
    }

    #[test]
    fn a_file_pointing_at_no_metadata_reads_as_no_tags() {
        let reader = DsfReader::new(Cursor::new(dsf_file(8, 96, 2))).expect("parses");

        assert_eq!(reader.tags(), &TrackTags::default());
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
