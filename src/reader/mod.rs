pub mod cue;
pub mod dff;
pub mod dsf;
pub mod flac;
pub mod sacd;
pub mod span;
pub mod tags;

use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::audio::{AudioFormat, PcmFormat};
use crate::dop;
use crate::dsd::DsdFormat;
use crate::reader::cue::{CueTrack, Sheet};
use crate::reader::dff::DffReader;
use crate::reader::dsf::DsfReader;
use crate::reader::flac::FlacReader;
use crate::reader::sacd::Disc;
use crate::reader::span::{SpanDsd, SpanPcm};
use crate::reader::tags::TrackTags;

/// DSD bytes per channel one DoP frame carries.
pub const DOP_BYTES_PER_FRAME: u64 = 2;

/// One recording to play: a file, or one track of something that describes a whole disc --
/// a SACD image, or a cue sheet over a file holding a whole side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRef {
    /// The file the track is named in: the image itself, or the cue sheet.
    pub path: PathBuf,
    /// Which track of that disc. `None` for a file that is one recording on its own.
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

    /// Narrow this to one track of a cue sheet: the same audio, bounded to the times the
    /// sheet gives and carrying the names it gives instead of the file's.
    fn narrowed(self, track: &CueTrack) -> Result<Self> {
        let tags = track.tags.clone();
        match self {
            Self::Dsd { source, .. } => {
                let rate = source.format().rate.hz();
                let total = source.total_bytes_per_channel();
                let end = track
                    .end
                    .map_or(total, |time| time.dsd_bytes(rate))
                    .min(total);
                let start = track.start.dsd_bytes(rate).min(end);
                Ok(Self::of_dsd(Box::new(SpanDsd::new(
                    source, start, end, tags,
                )?)))
            }
            Self::Pcm(source) => {
                let rate = source.format().rate;
                let total = source.total_frames();
                let end = track
                    .end
                    .map_or(total, |time| time.pcm_frames(rate))
                    .min(total);
                let start = track.start.pcm_frames(rate).min(end);
                Ok(Self::Pcm(Box::new(SpanPcm::new(source, start, end, tags)?)))
            }
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
        if cue::is_cue(&track.path) {
            return open_cue_track(&Sheet::read(&track.path)?, number);
        }
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

/// Open one track of a cue sheet: the file the sheet names, bounded to the track.
fn open_cue_track(sheet: &Sheet, number: u32) -> Result<Source> {
    let Some(track) = sheet.track(number) else {
        bail!(
            "{} lists tracks {}, so there is no track {number}",
            sheet.path.display(),
            sheet.numbering()
        );
    };
    let source = open(&TrackRef::file(track.file.clone())).with_context(|| {
        format!(
            "{} names {} as its audio",
            sheet.path.display(),
            track.file.display()
        )
    })?;
    source.narrowed(track)
}

/// Every recording a path holds, in playing order: one for a file, and one per track for a
/// disc image or for a file a cue sheet beside it splits.
pub fn tracks_of(path: &Path) -> Result<Vec<TrackRef>> {
    if sacd::is_image(path) {
        let disc = Disc::open(path)?;
        return Ok(disc
            .tracks()
            .iter()
            .map(|track| TrackRef::of_disc(path.to_path_buf(), track.number))
            .collect());
    }
    if cue::is_cue(path) {
        let sheet = Sheet::read(path)?;
        return Ok(cue_tracks(&sheet, &sheet.tracks));
    }
    match cue::sheet_for(path) {
        // Only this file's share of the sheet: a sheet naming several files describes one
        // album, and every file in it would otherwise queue the whole album again.
        Some(sheet) => Ok(cue_tracks(&sheet, sheet.tracks_in(path))),
        None => Ok(vec![TrackRef::file(path.to_path_buf())]),
    }
}

/// Name tracks of a cue sheet by the sheet rather than by the audio file, so that opening
/// one goes back through the sheet for its bounds.
pub fn cue_tracks<'a>(
    sheet: &Sheet,
    tracks: impl IntoIterator<Item = &'a CueTrack>,
) -> Vec<TrackRef> {
    let mut refs = Vec::new();
    for track in tracks {
        refs.push(TrackRef::of_disc(sheet.path.clone(), track.number));
    }
    refs
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
    use std::fs;
    use std::path::PathBuf;

    use crate::reader::dsf::tests::{BLOCK, dsf_file};
    use crate::reader::{DOP_BYTES_PER_FRAME, TrackRef, open, tracks_of};

    /// DSD bytes per channel one cue frame holds at DSD64.
    const CUE_FRAME_BYTES: u64 = 4_704;

    const SHEET: &str = "TITLE \"Kind of Blue\"\n\
                         PERFORMER \"Miles Davis\"\n\
                         FILE \"side.dsf\" WAVE\n\
                         TRACK 01 AUDIO\n  TITLE \"One\"\n  INDEX 01 00:00:00\n\
                         TRACK 02 AUDIO\n  TITLE \"Two\"\n  INDEX 01 00:00:01\n\
                         TRACK 03 AUDIO\n  TITLE \"Three\"\n  INDEX 01 00:00:02\n";

    /// A DSF holding three cue frames of audio, with a sheet beside it splitting it in three.
    fn side() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        let blocks = (CUE_FRAME_BYTES as usize * 3).div_ceil(BLOCK);
        let samples = (blocks * BLOCK * 8) as u64;
        fs::write(dir.path().join("side.dsf"), dsf_file(8, samples, blocks)).expect("audio");
        fs::write(dir.path().join("side.cue"), SHEET).expect("sheet");
        dir
    }

    #[test]
    fn a_file_a_sheet_splits_opens_as_the_tracks_the_sheet_names() {
        let dir = side();

        let tracks = tracks_of(&dir.path().join("side.dsf")).expect("lists");

        assert_eq!(tracks.len(), 3);
        assert!(
            tracks
                .iter()
                .all(|track| track.path == dir.path().join("side.cue"))
        );
        assert_eq!(tracks[1].number, Some(2));
    }

    #[test]
    fn each_cue_track_carries_its_own_stretch_of_the_file_and_the_sheets_names() {
        let dir = side();
        let tracks = tracks_of(&dir.path().join("side.dsf")).expect("lists");

        let second = open(&tracks[1]).expect("opens");

        assert_eq!(second.total_frames(), CUE_FRAME_BYTES / DOP_BYTES_PER_FRAME);
        assert_eq!(second.container(), "DSF");
        assert_eq!(second.tags().title.as_deref(), Some("Two"));
        assert_eq!(second.tags().artist.as_deref(), Some("Miles Davis"));
        assert_eq!(second.tags().album.as_deref(), Some("Kind of Blue"));
        assert_eq!(second.tags().track, Some(2));
    }

    #[test]
    fn the_last_cue_track_runs_to_the_end_of_the_file() {
        let dir = side();
        let tracks = tracks_of(&dir.path().join("side.dsf")).expect("lists");

        let last = open(&tracks[2]).expect("opens");

        assert_eq!(last.total_frames(), CUE_FRAME_BYTES / DOP_BYTES_PER_FRAME);
    }

    #[test]
    fn a_cue_track_reads_the_bytes_that_stretch_of_the_file_holds() {
        let dir = side();
        let tracks = tracks_of(&dir.path().join("side.dsf")).expect("lists");
        let whole = {
            let mut source = open(&TrackRef::file(dir.path().join("side.dsf"))).expect("opens");
            let mut out = Vec::new();
            while source.read(&mut out).expect("reads") > 0 {}
            out
        };

        let mut second = open(&tracks[1]).expect("opens");
        let mut out = Vec::new();
        while second.read(&mut out).expect("reads") > 0 {}

        let frames = (CUE_FRAME_BYTES / DOP_BYTES_PER_FRAME) as usize;
        let channels = 2;
        assert_eq!(out, whole[frames * channels..frames * 2 * channels]);
    }

    #[test]
    fn a_file_takes_only_its_own_share_of_a_sheet_that_names_several() {
        let dir = tempfile::tempdir().expect("temp dir");
        for name in ["one.dsf", "two.dsf"] {
            fs::write(dir.path().join(name), dsf_file(8, 96, 2)).expect("audio");
        }
        fs::write(
            dir.path().join("album.cue"),
            "FILE \"one.dsf\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n\
             FILE \"two.dsf\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n",
        )
        .expect("sheet");

        let first = tracks_of(&dir.path().join("one.dsf")).expect("lists");
        let second = tracks_of(&dir.path().join("two.dsf")).expect("lists");

        assert_eq!(first.len(), 1);
        assert_eq!(first[0].number, Some(1));
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].number, Some(2));
        assert_eq!(
            tracks_of(&dir.path().join("album.cue"))
                .expect("lists")
                .len(),
            2
        );
    }

    #[test]
    fn a_file_with_no_sheet_beside_it_stays_one_recording() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("track.dsf");
        fs::write(&path, dsf_file(8, 96, 2)).expect("audio");

        let tracks = tracks_of(&path).expect("lists");

        assert_eq!(tracks, [TrackRef::file(path)]);
    }

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
