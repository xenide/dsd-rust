use std::io::{Read, Seek, SeekFrom};

use anyhow::{Context, Result, bail, ensure};

use crate::dsd::{BitOrder, DsdFormat, DsdRate};
use crate::reader::tags::{TrackTags, parse_diin};
use crate::reader::{DsdSource, read_available};

const CHUNK_BYTES: usize = 4096;

/// Reader for Philips' DSDIFF container: big-endian chunks, byte-interleaved MSB-first DSD.
pub struct DffReader<R> {
    inner: R,
    format: DsdFormat,
    audio_bytes_per_channel: u64,
    /// Where the sound data chunk's body starts, so a seek can address the audio from it.
    data_start: u64,
    emitted: u64,
    scratch: Vec<u8>,
    tags: TrackTags,
}

struct ChunkHeader {
    id: [u8; 4],
    size: u64,
}

fn read_chunk_header<R: Read>(inner: &mut R) -> Result<Option<ChunkHeader>> {
    let mut header = [0_u8; 12];
    let filled = read_available(inner, &mut header)?;
    if filled == 0 {
        return Ok(None);
    }
    ensure!(filled == header.len(), "truncated chunk header");
    let id = header[0..4].try_into().expect("4 bytes in range");
    let size = u64::from_be_bytes(header[4..12].try_into().expect("8 bytes in range"));
    Ok(Some(ChunkHeader { id, size }))
}

/// DSDIFF pads every chunk body to an even length.
fn skip<R: Read>(inner: &mut R, bytes: u64) -> Result<()> {
    let padded = bytes + bytes % 2;
    let copied = std::io::copy(&mut inner.by_ref().take(padded), &mut std::io::sink())?;
    ensure!(copied == padded, "truncated chunk body");
    Ok(())
}

fn read_exact_vec<R: Read>(inner: &mut R, len: usize) -> Result<Vec<u8>> {
    let mut body = vec![0_u8; len];
    inner
        .read_exact(&mut body)
        .context("truncated chunk body")?;
    Ok(body)
}

impl<R: Read + Seek + Send + 'static> DffReader<R> {
    pub fn new(mut inner: R) -> Result<Self> {
        let mut form = [0_u8; 16];
        inner.read_exact(&mut form)?;
        ensure!(&form[0..4] == b"FRM8", "missing FRM8 chunk");
        ensure!(&form[12..16] == b"DSD ", "not a DSD form type");

        let mut walk = Walk::default();
        let scan = walk.run(&mut inner);
        // A file that ends mid-chunk after the audio still plays the audio it has, so only a
        // failure before the sound data was found is worth refusing the file over.
        if walk.sound.is_none() {
            scan?;
        }

        let (data_start, data_bytes) = walk.sound.context("no DSD sound data chunk found")?;
        let rate = walk.rate.context("no FS chunk giving the sample rate")?;
        let channels = walk
            .channels
            .context("no CHNL chunk giving the channel count")?;
        inner.seek(SeekFrom::Start(data_start))?;
        Ok(Self {
            inner,
            format: DsdFormat {
                rate: DsdRate::new(rate),
                channels,
            },
            audio_bytes_per_channel: data_bytes / u64::from(channels),
            data_start,
            emitted: 0,
            scratch: vec![0; CHUNK_BYTES * channels as usize],
            tags: walk.tags,
        })
    }
}

/// What one pass over the chunks in a DSDIFF form collects.
#[derive(Default)]
struct Walk {
    rate: Option<u32>,
    channels: Option<u16>,
    /// Where the sound data body starts, and how many bytes it declares.
    sound: Option<(u64, u64)>,
    tags: TrackTags,
}

impl Walk {
    /// Read every chunk in the form. The edited-master information sits either side of the
    /// sound data, so this runs to the end of the file rather than stopping at the audio.
    /// Stepping over the audio is a seek, and a seek past a truncated file ends the walk.
    fn run<R: Read + Seek>(&mut self, inner: &mut R) -> Result<()> {
        loop {
            let Some(chunk) = read_chunk_header(inner)? else {
                return Ok(());
            };
            match &chunk.id {
                b"PROP" => {
                    let body = self.read_body(inner, chunk.size)?;
                    let (rate, channels) = parse_properties(&body)?;
                    self.rate = rate;
                    self.channels = channels;
                }
                b"DIIN" => {
                    let body = self.read_body(inner, chunk.size)?;
                    parse_diin(&body, &mut self.tags);
                }
                b"DSD " => {
                    let start = inner.stream_position()?;
                    self.sound = Some((start, chunk.size));
                    inner.seek(SeekFrom::Start(start + chunk.size + chunk.size % 2))?;
                }
                b"DST " => bail!("DST compressed DSDIFF is not supported"),
                _ => skip(inner, chunk.size)?,
            }
        }
    }

    /// Read a chunk body and step over the pad byte an odd-length one carries.
    fn read_body<R: Read>(&self, inner: &mut R, size: u64) -> Result<Vec<u8>> {
        let body = read_exact_vec(inner, size as usize)?;
        if size % 2 == 1 {
            inner.read_exact(&mut [0_u8; 1])?;
        }
        Ok(body)
    }
}

/// Pull the sample rate and channel count out of a PROP/SND chunk body.
fn parse_properties(body: &[u8]) -> Result<(Option<u32>, Option<u16>)> {
    ensure!(
        body.len() >= 4 && &body[0..4] == b"SND ",
        "PROP chunk is not sound properties"
    );
    let mut rate = None;
    let mut channels = None;
    let mut offset = 4;
    while offset + 12 <= body.len() {
        let id = &body[offset..offset + 4];
        let size = u64::from_be_bytes(body[offset + 4..offset + 12].try_into().expect("8 bytes"));
        let start = offset + 12;
        let end = start
            .checked_add(size as usize)
            .filter(|end| *end <= body.len())
            .context("PROP sub-chunk runs past the chunk body")?;
        match id {
            b"FS  " => {
                ensure!(size == 4, "bad FS chunk size {size}");
                rate = Some(u32::from_be_bytes(
                    body[start..end].try_into().expect("4 bytes"),
                ));
            }
            b"CHNL" => {
                ensure!(size >= 2, "bad CHNL chunk size {size}");
                let count = u16::from_be_bytes(body[start..start + 2].try_into().expect("2 bytes"));
                ensure!(
                    (1..=6).contains(&count),
                    "unsupported channel count {count}"
                );
                channels = Some(count);
            }
            b"CMPR" => {
                ensure!(size >= 4, "bad CMPR chunk size {size}");
                let compression = &body[start..start + 4];
                ensure!(
                    compression == b"DSD ",
                    "unsupported DSDIFF compression {compression:?}"
                );
            }
            _ => {}
        }
        offset = end + end % 2;
    }
    Ok((rate, channels))
}

impl<R: Read + Seek + Send + 'static> DsdSource for DffReader<R> {
    fn container(&self) -> &'static str {
        "DSDIFF"
    }

    fn format(&self) -> DsdFormat {
        self.format
    }

    fn total_bytes_per_channel(&self) -> u64 {
        self.audio_bytes_per_channel
    }

    fn chunk_bytes(&self) -> usize {
        CHUNK_BYTES
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
        let wanted = (remaining as usize).min(CHUNK_BYTES) * channels;
        let filled = read_available(&mut self.inner, &mut self.scratch[..wanted])?;
        let count = filled / channels;
        if count == 0 {
            return Ok(0);
        }

        for (channel, plane) in planes.iter_mut().enumerate().take(channels) {
            for (index, byte) in plane[..count].iter_mut().enumerate() {
                *byte = self.scratch[index * channels + channel];
            }
            BitOrder::MsbFirst.normalize_to_msb_first(&mut plane[..count]);
        }
        self.emitted += count as u64;
        Ok(count)
    }

    /// DSDIFF interleaves the channels byte by byte, so every byte offset is a position.
    fn seek(&mut self, bytes_per_channel: u64) -> Result<u64> {
        let target = bytes_per_channel.min(self.audio_bytes_per_channel);
        let channels = u64::from(self.format.channels);
        self.inner
            .seek(SeekFrom::Start(self.data_start + target * channels))?;
        self.emitted = target;
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::dsd::DsdRate;
    use crate::reader::DsdSource;
    use crate::reader::dff::DffReader;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = Vec::from(*id);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    fn dff_file_with(audio_id: &[u8; 4], samples: &[u8]) -> Vec<u8> {
        let mut properties = Vec::from(*b"SND ");
        properties.extend_from_slice(&chunk(b"FS  ", &2_822_400_u32.to_be_bytes()));
        properties.extend_from_slice(&chunk(b"CHNL", &[0, 2, b'S', b'L', b'S', b'R']));
        properties.extend_from_slice(&chunk(b"CMPR", b"DSD \x0enot compressed"));

        let mut body = Vec::from(*b"DSD ");
        body.extend_from_slice(&chunk(b"FVER", &[1, 5, 0, 0]));
        body.extend_from_slice(&chunk(b"COMT", &[0, 0]));
        body.extend_from_slice(&chunk(b"PROP", &properties));
        body.extend_from_slice(&chunk(audio_id, samples));

        let mut file = Vec::from(*b"FRM8");
        file.extend_from_slice(&(body.len() as u64).to_be_bytes());
        file.extend_from_slice(&body);
        file
    }

    fn dff_file(samples: &[u8]) -> Vec<u8> {
        dff_file_with(b"DSD ", samples)
    }

    /// Put an edited-master information chunk after the audio, where the spec allows it and
    /// a walk that stopped at the sound data would never see it.
    fn dff_file_tagged_after_the_audio(samples: &[u8]) -> Vec<u8> {
        let mut text = Vec::from(11_u32.to_be_bytes());
        text.extend_from_slice(b"Peace Piece");
        let diin = chunk(b"DIIN", &chunk(b"DITI", &text));

        let mut file = dff_file(samples);
        file.extend_from_slice(&diin);
        let body = (file.len() - 12) as u64;
        file[4..12].copy_from_slice(&body.to_be_bytes());
        file
    }

    #[test]
    fn interleaved_bytes_are_split_into_channel_planes() {
        let samples: Vec<u8> = (0..64_u8).collect();
        let mut reader = DffReader::new(Cursor::new(dff_file(&samples))).expect("parses");

        assert_eq!(reader.format().rate, DsdRate::new(2_822_400));
        assert_eq!(reader.format().channels, 2);
        assert_eq!(reader.total_bytes_per_channel(), 32);

        let mut planes: Vec<Box<[u8]>> = vec![vec![0; reader.chunk_bytes()].into(); 2];
        let count = reader.read(&mut planes).expect("reads");

        assert_eq!(count, 32);
        assert_eq!(
            planes[0][..32],
            samples.iter().copied().step_by(2).collect::<Vec<_>>()[..]
        );
        assert_eq!(
            planes[1][..32],
            samples[1..].iter().copied().step_by(2).collect::<Vec<_>>()[..]
        );
        assert_eq!(reader.read(&mut planes).expect("reads"), 0);
    }

    #[test]
    fn a_seek_lands_on_the_exact_byte_and_reads_on_from_there() {
        let samples: Vec<u8> = (0..64_u8).collect();
        let mut reader = DffReader::new(Cursor::new(dff_file(&samples))).expect("parses");
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; reader.chunk_bytes()].into(); 2];

        let reached = reader.seek(10).expect("seeks");

        assert_eq!(reached, 10);
        let count = reader.read(&mut planes).expect("reads");
        assert_eq!(count, 22);
        assert_eq!(
            planes[0][..count],
            samples[20..].iter().copied().step_by(2).collect::<Vec<_>>()[..]
        );
    }

    #[test]
    fn a_seek_past_the_end_stops_at_the_end_of_the_audio() {
        let samples: Vec<u8> = (0..64_u8).collect();
        let mut reader = DffReader::new(Cursor::new(dff_file(&samples))).expect("parses");
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; reader.chunk_bytes()].into(); 2];

        let reached = reader.seek(u64::MAX).expect("seeks");

        assert_eq!(reached, 32);
        assert_eq!(reader.read(&mut planes).expect("reads"), 0);
    }

    #[test]
    fn an_information_chunk_after_the_audio_still_names_the_recording() {
        let samples: Vec<u8> = (0..64_u8).collect();
        let file = dff_file_tagged_after_the_audio(&samples);

        let mut reader = DffReader::new(Cursor::new(file)).expect("parses");

        assert_eq!(reader.tags().title.as_deref(), Some("Peace Piece"));
        // The read position still starts at the audio, not at whatever the walk ended on.
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; reader.chunk_bytes()].into(); 2];
        let count = reader.read(&mut planes).expect("reads");
        assert_eq!(count, 32);
        assert_eq!(planes[0][0], 0);
    }

    #[test]
    fn a_file_ending_mid_chunk_after_the_audio_still_plays() {
        let samples: Vec<u8> = (0..64_u8).collect();
        let mut file = dff_file_tagged_after_the_audio(&samples);
        file.truncate(file.len() - 6);

        let mut reader = DffReader::new(Cursor::new(file)).expect("parses");

        assert_eq!(reader.total_bytes_per_channel(), 32);
        let mut planes: Vec<Box<[u8]>> = vec![vec![0; reader.chunk_bytes()].into(); 2];
        assert_eq!(reader.read(&mut planes).expect("reads"), 32);
    }

    #[test]
    fn dst_compression_is_rejected_with_a_clear_message() {
        let file = dff_file_with(b"DST ", &[0; 8]);

        let error = DffReader::new(Cursor::new(file))
            .map(|_| ())
            .expect_err("rejects");

        assert!(error.to_string().contains("DST compressed"), "{error}");
    }
}
