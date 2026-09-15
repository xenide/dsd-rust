use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use claxon::frame::FrameReader;
use claxon::input::BufferedReader;

use crate::audio::{CARRIER_BITS, PcmFormat};
use crate::reader::PcmSource;
use crate::reader::tags::TrackTags;

const STREAMINFO_LEN: usize = 34;
const SEEK_POINT_LEN: usize = 18;
/// A seek point the encoder reserved but never filled in.
const SEEK_POINT_PLACEHOLDER: u64 = u64::MAX;
/// A metadata block larger than this is carrying artwork this player has no use for.
const MAX_BLOCK_BYTES: u32 = 4 << 20;

const BLOCK_STREAMINFO: u8 = 0;
const BLOCK_SEEKTABLE: u8 = 3;
const BLOCK_VORBIS_COMMENT: u8 = 4;

/// Where a seek table says a sample number sits in the file.
#[derive(Debug, Clone, Copy)]
struct SeekPoint {
    sample: u64,
    /// Bytes from the first audio frame, which is what the seek table counts from.
    offset: u64,
}

/// Everything ahead of the audio: the stream description, the seek table, and the tags.
struct Metadata {
    format: PcmFormat,
    total_frames: u64,
    max_block_frames: usize,
    seek_points: Vec<SeekPoint>,
    tags: TrackTags,
    audio_start: u64,
}

/// Reader for FLAC: claxon decodes the frames, and the metadata ahead of them is read here
/// because the seek table is what makes a jump cost one read instead of a whole decode.
pub struct FlacReader {
    /// Taken and rebuilt by a seek, which has to reposition the file underneath it.
    frames: Option<FrameReader<BufferedReader<File>>>,
    format: PcmFormat,
    total_frames: u64,
    seek_points: Vec<SeekPoint>,
    tags: TrackTags,
    audio_start: u64,
    /// Claxon's scratch buffer, handed in and out of every block so decoding allocates once.
    scratch: Vec<i32>,
    /// Interleaved carrier samples decoded but not yet handed out.
    pending: Vec<i32>,
    /// Frame index the first sample of `pending` sits at, and so where the next
    /// [`FlacReader::read`] starts.
    position: u64,
    /// Set while the decoder has been pointed somewhere new and the next frame header is
    /// the only thing that says where that is.
    resynced: bool,
    max_block_frames: usize,
}

impl FlacReader {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file =
            File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let metadata = read_metadata(&mut file)?;
        file.seek(SeekFrom::Start(metadata.audio_start))?;
        Ok(Self {
            frames: Some(FrameReader::new(BufferedReader::new(file))),
            format: metadata.format,
            total_frames: metadata.total_frames,
            seek_points: metadata.seek_points,
            tags: metadata.tags,
            audio_start: metadata.audio_start,
            scratch: Vec::new(),
            pending: Vec::new(),
            position: 0,
            resynced: true,
            max_block_frames: metadata.max_block_frames,
        })
    }

    /// Decode the next block into `pending`, or report the end of the stream.
    fn fill(&mut self) -> Result<bool> {
        let reader = self
            .frames
            .as_mut()
            .expect("the frame reader is only taken by a seek");
        let scratch = std::mem::take(&mut self.scratch);
        let Some(block) = reader.read_next_or_eof(scratch)? else {
            return Ok(false);
        };
        // Only a frame that follows a jump is asked where it sits. Reading on from one that
        // does is cheaper and, at the end of a stream whose blocks are a fixed size, more
        // accurate: a final short block reports a sample number scaled by its own length
        // rather than by the length the rest of the stream used.
        if self.resynced {
            self.position = block.time();
            self.resynced = false;
        }
        let channels = u32::from(self.format.channels);
        let planes: Vec<&[i32]> = (0..channels)
            .map(|channel| block.channel(channel))
            .collect();
        self.pending.reserve(block.len() as usize);
        // Claxon decodes a block channel by channel; the queue wants the frames interleaved.
        for index in 0..block.duration() as usize {
            for plane in &planes {
                self.pending.push(self.format.widen(plane[index]));
            }
        }
        self.scratch = block.into_buffer();
        Ok(true)
    }

    /// Point the decoder at `offset` bytes into the file, dropping what it had decoded.
    fn restart(&mut self, offset: u64) -> Result<()> {
        let reader = self
            .frames
            .take()
            .expect("the frame reader is put back below");
        let mut file = reader.into_inner().into_inner();
        file.seek(SeekFrom::Start(offset))?;
        self.frames = Some(FrameReader::new(BufferedReader::new(file)));
        self.pending.clear();
        self.resynced = true;
        Ok(())
    }

    /// The latest seek point at or before `frame`, or the start of the audio when the file
    /// carries no seek table or none of its points reach back that far.
    fn landing(&self, frame: u64) -> (u64, u64) {
        let point = self
            .seek_points
            .iter()
            .rev()
            .find(|point| point.sample <= frame);
        match point {
            Some(point) => (self.audio_start + point.offset, point.sample),
            None => (self.audio_start, 0),
        }
    }
}

impl PcmSource for FlacReader {
    fn container(&self) -> &'static str {
        "FLAC"
    }

    fn format(&self) -> PcmFormat {
        self.format
    }

    fn total_frames(&self) -> u64 {
        self.total_frames
    }

    fn tags(&self) -> &TrackTags {
        &self.tags
    }

    fn chunk_frames(&self) -> usize {
        self.max_block_frames
    }

    fn read(&mut self, out: &mut Vec<i32>) -> Result<usize> {
        if self.pending.is_empty() && !self.fill()? {
            return Ok(0);
        }
        let frames = self.pending.len() / self.format.channels as usize;
        out.append(&mut self.pending);
        self.position += frames as u64;
        Ok(frames)
    }

    /// FLAC is addressable at block boundaries, so a seek lands on the block holding the
    /// target. Where the file carries a seek table the jump is one read; where it does not,
    /// the blocks between here and there are decoded and dropped, so only a seek backwards
    /// through a tableless file costs the whole distance.
    fn seek(&mut self, frame: u64) -> Result<u64> {
        let target = frame.min(self.total_frames);
        let (offset, sample) = self.landing(target);
        if sample > self.position || target < self.position {
            self.restart(offset)?;
            self.position = sample;
        }
        loop {
            if self.pending.is_empty() && !self.fill()? {
                break;
            }
            let frames = (self.pending.len() / self.format.channels as usize) as u64;
            if self.position + frames > target {
                break;
            }
            self.position += frames;
            self.pending.clear();
        }
        Ok(self.position)
    }
}

fn read_metadata<R: Read>(reader: &mut R) -> Result<Metadata> {
    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;
    ensure!(&magic == b"fLaC", "missing fLaC marker");

    let mut stream = None;
    let mut seek_points = Vec::new();
    let mut tags = TrackTags::default();
    let mut audio_start = magic.len() as u64;
    loop {
        let mut header = [0_u8; 4];
        reader.read_exact(&mut header)?;
        let last = header[0] & 0x80 != 0;
        let kind = header[0] & 0x7F;
        let length = u32::from_be_bytes([0, header[1], header[2], header[3]]);
        audio_start += header.len() as u64 + u64::from(length);

        // Artwork and padding are read past rather than into memory.
        let known =
            kind == BLOCK_STREAMINFO || kind == BLOCK_SEEKTABLE || kind == BLOCK_VORBIS_COMMENT;
        let wanted = known && length <= MAX_BLOCK_BYTES;
        let mut body = vec![0_u8; if wanted { length as usize } else { 0 }];
        if wanted {
            reader.read_exact(&mut body)?;
        } else {
            std::io::copy(
                &mut (&mut *reader).take(u64::from(length)),
                &mut std::io::sink(),
            )?;
        }

        match kind {
            BLOCK_STREAMINFO => stream = Some(parse_streaminfo(&body)?),
            BLOCK_SEEKTABLE => seek_points = parse_seek_table(&body),
            BLOCK_VORBIS_COMMENT => tags = parse_vorbis_comment(&body),
            _ => {}
        }
        if last {
            break;
        }
    }

    let Some((format, total_frames, max_block_frames)) = stream else {
        bail!("no STREAMINFO block");
    };
    Ok(Metadata {
        format,
        total_frames,
        max_block_frames,
        seek_points,
        tags,
        audio_start,
    })
}

fn parse_streaminfo(body: &[u8]) -> Result<(PcmFormat, u64, usize)> {
    ensure!(
        body.len() >= STREAMINFO_LEN,
        "STREAMINFO is {} bytes, not {STREAMINFO_LEN}",
        body.len()
    );
    let max_block_frames = u16::from_be_bytes([body[2], body[3]]) as usize;
    let packed = u64::from_be_bytes(body[10..18].try_into().expect("8 bytes in range"));
    let rate = (packed >> 44) as u32;
    let channels = ((packed >> 41) & 0x7) as u16 + 1;
    let bits = ((packed >> 36) & 0x1F) as u32 + 1;
    let total_frames = packed & 0x0F_FFFF_FFFF;

    ensure!(rate > 0, "STREAMINFO declares a sample rate of 0");
    ensure!(
        max_block_frames > 0,
        "STREAMINFO declares a block size of 0"
    );
    ensure!(
        total_frames > 0,
        "the stream does not declare its length, which this player needs to size the transport"
    );
    ensure!(
        bits <= CARRIER_BITS,
        "{bits} bit FLAC cannot be carried without discarding {} bits per sample",
        bits - CARRIER_BITS
    );
    Ok((
        PcmFormat {
            rate,
            bits,
            channels,
        },
        total_frames,
        max_block_frames,
    ))
}

/// Seek points, dropping the placeholders an encoder leaves for points it never filled in.
fn parse_seek_table(body: &[u8]) -> Vec<SeekPoint> {
    let mut points = Vec::new();
    for entry in body.chunks_exact(SEEK_POINT_LEN) {
        let sample = u64::from_be_bytes(entry[0..8].try_into().expect("8 bytes in range"));
        if sample == SEEK_POINT_PLACEHOLDER {
            continue;
        }
        points.push(SeekPoint {
            sample,
            offset: u64::from_be_bytes(entry[8..16].try_into().expect("8 bytes in range")),
        });
    }
    points.sort_by_key(|point| point.sample);
    points
}

/// Read the fields this player displays out of a Vorbis comment. Everything in it is
/// little-endian, which is the one place FLAC departs from big-endian.
fn parse_vorbis_comment(body: &[u8]) -> TrackTags {
    let mut tags = TrackTags::default();
    let Some((_vendor, rest)) = take_field(body) else {
        return tags;
    };
    let Some((count, mut rest)) = split_u32(rest) else {
        return tags;
    };
    for _ in 0..count {
        let Some((field, remainder)) = take_field(rest) else {
            return tags;
        };
        rest = remainder;
        let Ok(text) = std::str::from_utf8(field) else {
            continue;
        };
        let Some((name, value)) = text.split_once('=') else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        match name.to_ascii_uppercase().as_str() {
            "TITLE" => tags.title = Some(value.to_owned()),
            "ARTIST" => tags.artist = Some(value.to_owned()),
            "ALBUM" => tags.album = Some(value.to_owned()),
            // A "4/12" style number counts the tracks after the slash, which is not a number.
            "TRACKNUMBER" => {
                tags.track = value.split('/').next().and_then(|n| n.parse().ok());
            }
            _ => {}
        }
    }
    tags
}

fn split_u32(body: &[u8]) -> Option<(u32, &[u8])> {
    let (head, rest) = body.split_at_checked(4)?;
    Some((
        u32::from_le_bytes(head.try_into().expect("4 bytes in range")),
        rest,
    ))
}

/// One length-prefixed field, and whatever follows it.
fn take_field(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let (length, rest) = split_u32(body)?;
    rest.split_at_checked(length as usize)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::reader::flac::{parse_streaminfo, parse_vorbis_comment};
    use crate::reader::tags::TrackTags;

    /// A STREAMINFO body describing one stream, with no MD5.
    pub(crate) fn streaminfo(rate: u32, bits: u32, channels: u16, frames: u64) -> Vec<u8> {
        let mut body = vec![0_u8; 34];
        body[0..2].copy_from_slice(&4096_u16.to_be_bytes());
        body[2..4].copy_from_slice(&4096_u16.to_be_bytes());
        let packed = (u64::from(rate) << 44)
            | (u64::from(channels - 1) << 41)
            | (u64::from(bits - 1) << 36)
            | frames;
        body[10..18].copy_from_slice(&packed.to_be_bytes());
        body
    }

    /// A Vorbis comment body carrying `fields`, as FLAC lays one out.
    fn vorbis_comment(fields: &[&str]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&0_u32.to_le_bytes());
        body.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        for field in fields {
            body.extend_from_slice(&(field.len() as u32).to_le_bytes());
            body.extend_from_slice(field.as_bytes());
        }
        body
    }

    #[test]
    fn streaminfo_yields_the_rate_width_and_channel_count() {
        let (format, frames, block) =
            parse_streaminfo(&streaminfo(96_000, 24, 2, 18_026_240)).expect("parses");

        assert_eq!(format.rate, 96_000);
        assert_eq!(format.bits, 24);
        assert_eq!(format.channels, 2);
        assert_eq!(frames, 18_026_240);
        assert_eq!(block, 4096);
    }

    #[test]
    fn a_stream_of_unknown_length_is_refused_with_a_reason() {
        let error = parse_streaminfo(&streaminfo(44_100, 16, 2, 0)).expect_err("refused");

        assert!(error.to_string().contains("does not declare its length"));
    }

    #[test]
    fn a_width_past_the_carrier_is_refused_rather_than_truncated() {
        let error = parse_streaminfo(&streaminfo(44_100, 32, 2, 100)).expect_err("refused");

        assert!(error.to_string().contains("32 bit FLAC"), "{error}");
    }

    #[test]
    fn the_tags_this_player_shows_are_read_whatever_case_they_are_written_in() {
        let body = vorbis_comment(&[
            "ALBUM=Sob Rock",
            "artist=John Mayer",
            "TITLE=Last Train Home",
            "TRACKNUMBER=1/10",
            "REPLAYGAIN_TRACK_GAIN=-3.4 dB",
        ]);

        let tags = parse_vorbis_comment(&body);

        assert_eq!(tags.title.as_deref(), Some("Last Train Home"));
        assert_eq!(tags.artist.as_deref(), Some("John Mayer"));
        assert_eq!(tags.album.as_deref(), Some("Sob Rock"));
        assert_eq!(tags.track, Some(1));
    }

    #[test]
    fn an_empty_or_truncated_comment_leaves_the_file_untagged_rather_than_failing() {
        assert_eq!(parse_vorbis_comment(&[]), TrackTags::default());
        assert_eq!(
            parse_vorbis_comment(&[0, 0, 0, 0, 9, 9]),
            TrackTags::default()
        );
    }
}
