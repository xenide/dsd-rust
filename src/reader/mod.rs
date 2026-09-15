pub mod dff;
pub mod dsf;
pub mod flac;
pub mod sacd;
pub mod tags;

use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::audio::{AudioFormat, PcmFormat};
use crate::dop;
use crate::dsd::DsdFormat;
use crate::reader::dff::DffReader;
use crate::reader::dsf::DsfReader;
use crate::reader::flac::FlacReader;
use crate::reader::sacd::Disc;
use crate::reader::tags::TrackTags;

/// DSD bytes per channel one DoP frame carries.
pub const DOP_BYTES_PER_FRAME: u64 = 2;

/// One recording to play: a file, or one track of a container that holds a whole disc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRef {
    pub path: PathBuf,
    /// Which track of a disc image. `None` for a file that is one recording on its own.
    pub number: Option<u32>,
}

impl TrackRef {
    pub fn file(path: PathBuf) -> Self {
        Self { path, number: None }
    }

    pub const fn of_disc(path: PathBuf, number: u32) -> Self {
        Self {
            path,
            number: Some(number),
        }
    }

    /// What to call this when a file name is all there is to go on.
    pub fn label(&self) -> String {
        let name = self
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        match self.number {
            Some(number) => format!("{name} track {number}"),
            None => name,
        }
    }
}

impl fmt::Display for TrackRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.number {
            Some(number) => write!(f, "{} track {number}", self.path.display()),
            None => self.path.display().fmt(f),
        }
    }
}

/// A source of planar, MSB-first DSD bytes.
pub trait DsdSource: Send {
    fn container(&self) -> &'static str;

    fn format(&self) -> DsdFormat;

    /// DSD bytes of audio per channel, excluding container padding.
    fn total_bytes_per_channel(&self) -> u64;

    /// What the container says the recording is. Empty when it carries no tags.
    fn tags(&self) -> &TrackTags;

    /// Bytes each plane passed to [`DsdSource::read`] must hold.
    fn chunk_bytes(&self) -> usize;

    /// Fill the head of each plane with the next DSD bytes. Returns bytes per plane, 0 at EOF.
    fn read(&mut self, planes: &mut [Box<[u8]>]) -> Result<usize>;

    /// Move the read position to `bytes_per_channel` from the start of the audio, clamped to
    /// the file. Returns where it landed, which a container addressable only in whole blocks
    /// rounds down.
    fn seek(&mut self, bytes_per_channel: u64) -> Result<u64>;
}

/// A source of interleaved linear PCM, already widened to the carrier word.
pub trait PcmSource: Send {
    fn container(&self) -> &'static str;

    fn format(&self) -> PcmFormat;

    /// Frames of audio the whole recording holds.
    fn total_frames(&self) -> u64;

    fn tags(&self) -> &TrackTags;

    /// Frames one [`PcmSource::read`] may return at most, for sizing the queue.
    fn chunk_frames(&self) -> usize;

    /// Append the next interleaved frames to `out`. Returns frames appended, 0 at EOF.
    fn read(&mut self, out: &mut Vec<i32>) -> Result<usize>;

    /// Move the read position to `frame`, clamped to the recording. Returns where it landed.
    fn seek(&mut self, frame: u64) -> Result<u64>;
}

/// An open recording, whichever of the two things a file can hold.
///
/// Both come out as interleaved carrier samples: 24-bit DoP words for DSD, and for PCM the
/// container's own codes left-justified in the same width. One queue and one render callback
/// then serve both.
pub enum Source {
    Dsd {
        source: Box<dyn DsdSource>,
        /// Scratch the DSD lands in before it is packed into DoP frames.
        planes: Vec<Box<[u8]>>,
    },
    Pcm(Box<dyn PcmSource>),
}

impl Source {
    fn of_dsd(source: Box<dyn DsdSource>) -> Self {
        let channels = source.format().channels as usize;
        let planes = vec![vec![0_u8; source.chunk_bytes()].into(); channels];
        Self::Dsd { source, planes }
    }

    pub fn container(&self) -> &'static str {
        match self {
            Self::Dsd { source, .. } => source.container(),
            Self::Pcm(source) => source.container(),
        }
    }

    pub fn format(&self) -> AudioFormat {
        match self {
            Self::Dsd { source, .. } => AudioFormat::Dsd(source.format()),
            Self::Pcm(source) => AudioFormat::Pcm(source.format()),
        }
    }

    pub fn tags(&self) -> &TrackTags {
        match self {
            Self::Dsd { source, .. } => source.tags(),
            Self::Pcm(source) => source.tags(),
        }
    }

    /// Carrier frames the whole recording holds: one per two DSD bytes, or one per PCM frame.
    pub fn total_frames(&self) -> u64 {
        match self {
            Self::Dsd { source, .. } => source.total_bytes_per_channel() / DOP_BYTES_PER_FRAME,
            Self::Pcm(source) => source.total_frames(),
        }
    }

    /// Carrier frames one [`Source::read`] may return at most.
    pub fn chunk_frames(&self) -> usize {
        match self {
            Self::Dsd { source, .. } => source.chunk_bytes().div_ceil(DOP_BYTES_PER_FRAME as usize),
            Self::Pcm(source) => source.chunk_frames(),
        }
    }

    pub fn duration_secs(&self) -> f64 {
        self.total_frames() as f64 / f64::from(self.format().carrier_rate())
    }

    /// Append the next carrier frames to `out`. Returns frames appended, 0 at the end.
    pub fn read(&mut self, out: &mut Vec<i32>) -> Result<usize> {
        match self {
            Self::Dsd { source, planes } => {
                let count = source.read(planes)?;
                if count == 0 {
                    return Ok(0);
                }
                let slices: Vec<&[u8]> = planes.iter().map(|plane| &plane[..count]).collect();
                Ok(dop::pack_planes(&slices, out))
            }
            Self::Pcm(source) => source.read(out),
        }
    }

    /// Move to `frame`, clamped to the recording, and report where it landed.
    pub fn seek(&mut self, frame: u64) -> Result<u64> {
        match self {
            Self::Dsd { source, .. } => {
                Ok(source.seek(frame * DOP_BYTES_PER_FRAME)? / DOP_BYTES_PER_FRAME)
            }
            Self::Pcm(source) => source.seek(frame),
        }
    }

    /// Hand the DSD source over to the native USB path, which carries the bytes itself
    /// rather than wrapping them in DoP frames.
    pub fn into_dsd(self) -> Option<Box<dyn DsdSource>> {
        match self {
            Self::Dsd { source, .. } => Some(source),
            Self::Pcm(_) => None,
        }
    }
}

/// Open a recording, dispatching on the container magic rather than the extension.
pub fn open(track: &TrackRef) -> Result<Source> {
    if let Some(number) = track.number {
        return Ok(Source::of_dsd(Box::new(
            Disc::open(&track.path)?.reader(number)?,
        )));
    }

    let path = track.path.as_path();
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 16, file);

    let mut magic = [0_u8; 4];
    reader
        .read_exact(&mut magic)
        .with_context(|| format!("{} is empty", path.display()))?;
    reader.seek(SeekFrom::Start(0))?;

    let context = || format!("cannot parse {}", path.display());
    match &magic {
        b"DSD " => Ok(Source::of_dsd(Box::new(
            DsfReader::new(reader).with_context(context)?,
        ))),
        b"FRM8" => Ok(Source::of_dsd(Box::new(
            DffReader::new(reader).with_context(context)?,
        ))),
        b"fLaC" => Ok(Source::Pcm(Box::new(
            FlacReader::open(path).with_context(context)?,
        ))),
        // An image is a whole disc rather than one recording, so a track has to be named.
        // `tracks_of` is what names them, and both `play` and the file browser go through it.
        _ if sacd::is_image(path) => bail!(
            "{}: a SACD image holds a whole disc, so it opens as the tracks inside it",
            path.display()
        ),
        other => bail!(
            "{}: unrecognised container (magic {:02X?}); expected DSF, DSDIFF, FLAC, or a \
             SACD image",
            path.display(),
            other
        ),
    }
}

/// Every recording a path holds, in playing order: one for a file, and one per track for a
/// disc image.
pub fn tracks_of(path: &Path) -> Result<Vec<TrackRef>> {
    if !sacd::is_image(path) {
        return Ok(vec![TrackRef::file(path.to_path_buf())]);
    }
    let disc = Disc::open(path)?;
    Ok(disc
        .tracks()
        .iter()
        .map(|track| TrackRef::of_disc(path.to_path_buf(), track.number))
        .collect())
}

/// Read only what a recording says it is, for a listing with no reason to keep the audio open.
pub fn tags_of(track: &TrackRef) -> Result<TrackTags> {
    Ok(open(track)?.tags().clone())
}

/// Read until `buf` is full or the stream ends, returning how many bytes arrived.
pub(crate) fn read_available<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let read = reader.read(&mut buf[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::reader::TrackRef;

    #[test]
    fn a_disc_track_names_itself_by_file_and_number() {
        let track = TrackRef::of_disc(PathBuf::from("/music/disc.iso"), 3);

        assert_eq!(track.label(), "disc.iso track 3");
        assert_eq!(track.to_string(), "/music/disc.iso track 3");
    }

    #[test]
    fn a_plain_file_names_itself_by_file_alone() {
        let track = TrackRef::file(PathBuf::from("/music/track.dsf"));

        assert_eq!(track.label(), "track.dsf");
        assert_eq!(track.to_string(), "/music/track.dsf");
    }
}
