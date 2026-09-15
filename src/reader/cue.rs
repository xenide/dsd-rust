//! Cue sheets: the track list for a file that holds a whole side in one stream.
//!
//! A rip of a disc to one DSDIFF, DSF, or FLAC file keeps its track boundaries in a `.cue`
//! sheet beside it. The sheet names the audio file and gives each track a start time, so the
//! file opens as the tracks the sheet describes rather than as one recording.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use tracing::debug;

use crate::reader::tags::TrackTags;

/// A cue sheet counts in frames of a seventy-fifth of a second, as a CD does.
const CUE_FRAME_RATE: u64 = 75;
const BITS_PER_BYTE: u64 = 8;
/// A sheet larger than this is not a track list, so stop before reading it into memory.
const MAX_SHEET_BYTES: u64 = 4 << 20;
/// Containers a sheet's `FILE` may name once the rip changed its format but not the sheet.
const AUDIO_EXTENSIONS: [&str; 3] = ["dsf", "dff", "flac"];

/// A position in a sheet, in frames of a seventy-fifth of a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CueTime(u64);

impl CueTime {
    /// DSD bytes per channel this time sits at. Every DSD rate is a whole number of bytes
    /// per cue frame, so the boundary lands exactly where the sheet put it.
    pub const fn dsd_bytes(self, rate_hz: u32) -> u64 {
        self.0 * rate_hz as u64 / (BITS_PER_BYTE * CUE_FRAME_RATE)
    }

    pub const fn pcm_frames(self, rate: u32) -> u64 {
        self.0 * rate as u64 / CUE_FRAME_RATE
    }

    /// Parse `MM:SS:FF`, or `MM:SS` from a sheet that leaves the frames off.
    fn parse(text: &str) -> Result<Self> {
        let mut parts = text.split(':');
        let mut value = 0_u64;
        for (index, unit) in [CUE_FRAME_RATE * 60, CUE_FRAME_RATE, 1]
            .into_iter()
            .enumerate()
        {
            let Some(part) = parts.next() else {
                if index == 2 {
                    break;
                }
                bail!("{text} is not a cue sheet time of the form MM:SS:FF");
            };
            let part: u64 = part
                .trim()
                .parse()
                .with_context(|| format!("{text} is not a cue sheet time of the form MM:SS:FF"))?;
            value += part * unit;
        }
        Ok(Self(value))
    }
}

/// One track of a sheet: where in which file it starts, where it ends, and what it is called.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CueTrack {
    pub number: u32,
    /// The audio file this track sits in, resolved next to the sheet.
    pub file: PathBuf,
    pub start: CueTime,
    /// Where the next track of the same file starts. `None` for the last track of a file,
    /// which runs to the end of it.
    pub end: Option<CueTime>,
    pub tags: TrackTags,
}

/// A parsed cue sheet, with its tracks flattened across however many files it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sheet {
    pub path: PathBuf,
    pub tracks: Vec<CueTrack>,
}

impl Sheet {
    pub fn read(path: &Path) -> Result<Self> {
        let size = std::fs::metadata(path)
            .with_context(|| format!("cannot open {}", path.display()))?
            .len();
        if size > MAX_SHEET_BYTES {
            bail!("{} is too large to be a cue sheet", path.display());
        }
        let bytes =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let tracks = parse(&text_of(bytes), folder_of(path))
            .with_context(|| format!("cannot parse {}", path.display()))?;
        if tracks.is_empty() {
            bail!("{} lists no audio tracks", path.display());
        }
        Ok(Self {
            path: path.to_path_buf(),
            tracks,
        })
    }

    pub fn track(&self, number: u32) -> Option<&CueTrack> {
        self.tracks.iter().find(|track| track.number == number)
    }

    /// The track numbers the sheet gives, for saying which ones it does not.
    pub fn numbering(&self) -> String {
        let mut numbers = Vec::with_capacity(self.tracks.len());
        for track in &self.tracks {
            numbers.push(track.number.to_string());
        }
        numbers.join(", ")
    }

    /// True when this sheet is the track list for `audio`.
    ///
    /// Only the file name is compared, because a sheet is looked for in the audio file's own
    /// folder and that is where its own `FILE` was resolved.
    pub fn covers(&self, audio: &Path) -> bool {
        !self.tracks_in(audio).is_empty()
    }

    /// The tracks the sheet puts in one of the files it names. A sheet that names several
    /// still describes one album, but each file holds only its own share of it.
    pub fn tracks_in(&self, audio: &Path) -> Vec<&CueTrack> {
        let Some(name) = audio.file_name() else {
            return Vec::new();
        };
        let mut tracks = Vec::new();
        for track in &self.tracks {
            if track.file.file_name() == Some(name) {
                tracks.push(track);
            }
        }
        tracks
    }
}

pub fn is_cue(path: &Path) -> bool {
    let Some(extension) = path.extension() else {
        return false;
    };
    extension.eq_ignore_ascii_case("cue")
}

/// Every cue sheet in a folder that parses. One that does not is not worth refusing to list
/// the folder over, so it drops out silently and its audio file lists as one recording.
pub fn sheets_in(dir: &Path) -> Vec<Sheet> {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut sheets = Vec::new();
    for entry in listing.flatten() {
        let path = entry.path();
        if !is_cue(&path) {
            continue;
        }
        match Sheet::read(&path) {
            Ok(sheet) => sheets.push(sheet),
            Err(error) => debug!("{error:#}"),
        }
    }
    sheets.sort_by(|a, b| a.path.cmp(&b.path));
    sheets
}

/// The cue sheet in an audio file's own folder that names it, if one does.
pub fn sheet_for(audio: &Path) -> Option<Sheet> {
    sheets_in(folder_of(audio))
        .into_iter()
        .find(|sheet| sheet.covers(audio))
}

fn folder_of(path: &Path) -> &Path {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    }
}

/// A sheet is written in whatever the ripper used. UTF-8 covers most of them, and the rest
/// are read as Latin-1, which at worst spells an accent wrong rather than failing to open.
fn text_of(bytes: Vec<u8>) -> String {
    let mut bytes = bytes;
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        bytes.drain(..3);
    }
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => error.into_bytes().into_iter().map(char::from).collect(),
    }
}

/// What the parser has read so far about one track, before the next track says where it ends.
struct Pending {
    number: u32,
    file: PathBuf,
    /// Which `FILE` the track belongs to, so a sheet naming several does not run a track
    /// on into the start time of one in the next file.
    file_index: usize,
    index0: Option<CueTime>,
    index1: Option<CueTime>,
    tags: TrackTags,
}

impl Pending {
    /// Where the audio starts: the track proper, or the pregap for a sheet that gives no
    /// `INDEX 01`.
    fn start(&self) -> Option<CueTime> {
        self.index1.or(self.index0)
    }

    fn finish(self, end: Option<CueTime>, album: &TrackTags) -> Option<CueTrack> {
        let start = self.start()?;
        let mut tags = self.tags;
        tags.track = Some(self.number);
        tags.album = album.album.clone();
        if tags.artist.is_none() {
            tags.artist = album.artist.clone();
        }
        Some(CueTrack {
            number: self.number,
            file: self.file,
            start,
            end,
            tags,
        })
    }
}

fn parse(text: &str, dir: &Path) -> Result<Vec<CueTrack>> {
    let mut album = TrackTags::default();
    let mut pending: Vec<Pending> = Vec::new();
    let mut file = None;
    let mut file_index = 0;
    for line in text.lines() {
        let tokens = tokenise(line);
        let Some((command, arguments)) = tokens.split_first() else {
            continue;
        };
        match command.to_ascii_uppercase().as_str() {
            "FILE" => {
                let Some(name) = arguments.first() else {
                    bail!("a FILE line names no file");
                };
                file_index += 1;
                file = Some(resolve(dir, name));
            }
            "TRACK" => open_track(&mut pending, arguments, file.as_ref(), file_index)?,
            "TITLE" => assign(&mut pending, &mut album, arguments, |tags, text| {
                tags.title = Some(text);
            }),
            "PERFORMER" => assign(&mut pending, &mut album, arguments, |tags, text| {
                tags.artist = Some(text);
            }),
            "INDEX" => set_index(&mut pending, arguments)?,
            _ => {}
        }
    }
    // The album title names the album; a track's own TITLE is what names the track.
    album.album = album.title.take();
    Ok(link(pending, &album))
}

/// An audio track starts collecting its own tags. A data track is skipped, so that the audio
/// tracks either side of it keep their numbers and the data never plays.
fn open_track(
    pending: &mut Vec<Pending>,
    arguments: &[&str],
    file: Option<&PathBuf>,
    file_index: usize,
) -> Result<()> {
    let [number, mode, ..] = arguments else {
        bail!("a TRACK line gives no number and mode");
    };
    if !mode.eq_ignore_ascii_case("AUDIO") {
        return Ok(());
    }
    let number: u32 = number
        .parse()
        .with_context(|| format!("{number} is not a track number"))?;
    let Some(file) = file else {
        bail!("track {number} comes before any FILE line");
    };
    pending.push(Pending {
        number,
        file: file.clone(),
        file_index,
        index0: None,
        index1: None,
        tags: TrackTags::default(),
    });
    Ok(())
}

/// Text before the first track names the album; text after one names that track.
fn assign(
    pending: &mut [Pending],
    album: &mut TrackTags,
    arguments: &[&str],
    set: fn(&mut TrackTags, String),
) {
    let Some(text) = arguments.first().map(|text| text.trim()) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    match pending.last_mut() {
        Some(track) => set(&mut track.tags, text.to_owned()),
        None => set(album, text.to_owned()),
    }
}

fn set_index(pending: &mut [Pending], arguments: &[&str]) -> Result<()> {
    let [number, time, ..] = arguments else {
        return Ok(());
    };
    let Some(track) = pending.last_mut() else {
        return Ok(());
    };
    let time = CueTime::parse(time)?;
    match number.parse::<u32>() {
        Ok(0) => track.index0 = Some(time),
        Ok(1) => track.index1 = Some(time),
        Ok(_) | Err(_) => {}
    }
    Ok(())
}

/// Close each track at the start of the next one in the same file. A track with no start
/// time at all describes nothing playable, so it drops out rather than playing the rest of
/// the file.
fn link(pending: Vec<Pending>, album: &TrackTags) -> Vec<CueTrack> {
    let ends: Vec<Option<CueTime>> = pending
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let next = pending.get(index + 1)?;
            if next.file_index == track.file_index {
                next.start()
            } else {
                None
            }
        })
        .collect();
    let mut tracks = Vec::with_capacity(pending.len());
    for (track, end) in pending.into_iter().zip(ends) {
        if let Some(track) = track.finish(end, album) {
            tracks.push(track);
        }
    }
    tracks
}

/// Where a sheet's `FILE` actually is: beside the sheet under the name it gives, or under the
/// same stem in a container this player reads, for a rip whose sheet still names the source.
fn resolve(dir: &Path, name: &str) -> PathBuf {
    let named = dir.join(name.replace('\\', "/"));
    if named.is_file() {
        return named;
    }
    for extension in AUDIO_EXTENSIONS {
        let candidate = named.with_extension(extension);
        if candidate.is_file() {
            return candidate;
        }
    }
    named
}

/// Split a line into its command and arguments, keeping a quoted argument whole.
fn tokenise(line: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if bytes[index] == b'"' {
            let start = index + 1;
            let end = line[start..]
                .find('"')
                .map_or(line.len(), |offset| start + offset);
            tokens.push(&line[start..end]);
            index = (end + 1).min(line.len());
            continue;
        }
        let start = index;
        while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        tokens.push(&line[start..index]);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use crate::reader::cue::{CueTime, Sheet, is_cue, parse, sheet_for, tokenise};

    const SHEET: &str = r#"PERFORMER "Miles Davis"
TITLE "Kind of Blue"
FILE "album.dff" WAVE
  TRACK 01 AUDIO
    TITLE "So What"
    INDEX 01 00:00:00
  TRACK 02 AUDIO
    TITLE "Freddie Freeloader"
    PERFORMER "The Sextet"
    INDEX 00 09:20:00
    INDEX 01 09:22:37
  TRACK 03 AUDIO
    TITLE "Blue in Green"
    INDEX 01 19:00:00
"#;

    fn tracks(text: &str) -> Vec<crate::reader::cue::CueTrack> {
        parse(text, Path::new("/music")).expect("parses")
    }

    #[test]
    fn each_track_runs_from_its_own_index_to_the_next_ones() {
        let tracks = tracks(SHEET);

        assert_eq!(tracks.len(), 3);
        assert_eq!(tracks[0].start, CueTime(0));
        assert_eq!(tracks[0].end, Some(CueTime(9 * 60 * 75 + 22 * 75 + 37)));
        assert_eq!(tracks[1].start, CueTime(9 * 60 * 75 + 22 * 75 + 37));
        assert_eq!(tracks[1].end, Some(CueTime(19 * 60 * 75)));
        assert_eq!(tracks[2].end, None);
        assert_eq!(tracks[0].file, Path::new("/music/album.dff"));
    }

    #[test]
    fn a_track_takes_the_albums_names_where_it_gives_none_of_its_own() {
        let tracks = tracks(SHEET);

        assert_eq!(tracks[0].tags.title.as_deref(), Some("So What"));
        assert_eq!(tracks[0].tags.artist.as_deref(), Some("Miles Davis"));
        assert_eq!(tracks[0].tags.album.as_deref(), Some("Kind of Blue"));
        assert_eq!(tracks[0].tags.track, Some(1));
        assert_eq!(tracks[1].tags.artist.as_deref(), Some("The Sextet"));
    }

    #[test]
    fn a_data_track_is_skipped_and_the_audio_either_side_keeps_its_number() {
        let text = "FILE \"album.flac\" WAVE\n\
                    TRACK 01 AUDIO\nINDEX 01 00:00:00\n\
                    TRACK 02 MODE1/2048\nINDEX 01 05:00:00\n\
                    TRACK 03 AUDIO\nINDEX 01 06:00:00\n";

        let tracks = tracks(text);

        let numbers: Vec<u32> = tracks.iter().map(|track| track.number).collect();
        assert_eq!(numbers, [1, 3]);
        assert_eq!(tracks[0].end, Some(CueTime(6 * 60 * 75)));
    }

    #[test]
    fn a_track_does_not_run_on_into_the_next_file_the_sheet_names() {
        let text = "FILE \"one.dsf\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n\
                    FILE \"two.dsf\" WAVE\nTRACK 02 AUDIO\nINDEX 01 00:00:00\n";

        let tracks = tracks(text);

        assert_eq!(tracks[0].end, None);
        assert_eq!(tracks[0].file, Path::new("/music/one.dsf"));
        assert_eq!(tracks[1].file, Path::new("/music/two.dsf"));
    }

    #[test]
    fn a_cue_frame_is_a_whole_number_of_dsd_bytes_and_pcm_frames() {
        let one_frame = CueTime(1);

        assert_eq!(one_frame.dsd_bytes(2_822_400), 4_704);
        assert_eq!(one_frame.dsd_bytes(11_289_600), 18_816);
        assert_eq!(one_frame.pcm_frames(44_100), 588);
        assert_eq!(one_frame.pcm_frames(192_000), 2_560);
    }

    #[test]
    fn a_time_with_no_frame_field_still_parses() {
        assert_eq!(CueTime::parse("01:30").expect("parses"), CueTime(90 * 75));
        assert_eq!(
            CueTime::parse("100:00:00").expect("parses").0,
            100 * 60 * 75
        );
        assert!(CueTime::parse("halfway").is_err());
    }

    #[test]
    fn quoted_arguments_stay_whole_and_bare_ones_split_on_spaces() {
        assert_eq!(
            tokenise("  FILE \"a long name.dff\" WAVE  "),
            ["FILE", "a long name.dff", "WAVE"]
        );
        assert_eq!(tokenise("TRACK 01 AUDIO"), ["TRACK", "01", "AUDIO"]);
        assert!(tokenise("   ").is_empty());
    }

    #[test]
    fn a_sheet_is_found_from_the_audio_file_it_names() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("album.dff"), b"audio").expect("audio");
        fs::write(dir.path().join("album.cue"), SHEET).expect("sheet");

        let sheet = sheet_for(&dir.path().join("album.dff")).expect("sheet");

        assert_eq!(sheet.path, dir.path().join("album.cue"));
        assert_eq!(sheet.track(2).expect("track").number, 2);
        assert!(sheet.track(9).is_none());
        assert!(is_cue(&sheet.path));
    }

    #[test]
    fn a_sheet_naming_a_container_that_was_converted_finds_the_file_beside_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("album.dsf"), b"audio").expect("audio");
        fs::write(
            dir.path().join("album.cue"),
            "FILE \"album.wav\" WAVE\nTRACK 01 AUDIO\nINDEX 01 00:00:00\n",
        )
        .expect("sheet");

        let sheet = Sheet::read(&dir.path().join("album.cue")).expect("reads");

        assert_eq!(sheet.tracks[0].file, dir.path().join("album.dsf"));
        assert!(sheet.covers(&dir.path().join("album.dsf")));
        assert_eq!(sheet.tracks_in(&dir.path().join("album.dsf")).len(), 1);
        assert!(sheet.tracks_in(&dir.path().join("other.dsf")).is_empty());
    }

    #[test]
    fn a_file_with_no_sheet_beside_it_finds_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("album.dff"), b"audio").expect("audio");

        assert!(sheet_for(&dir.path().join("album.dff")).is_none());
    }
}
