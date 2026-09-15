//! The Scarlet Book table of contents: what a SACD image says it holds and where.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use anyhow::{Result, bail, ensure};

use crate::dsd::DsdRate;
use crate::reader::tags::TrackTags;

/// One logical block of a SACD.
pub const SECTOR: usize = 2048;
/// Audio frames a second, which is also what a frame timecode counts in.
pub const FRAME_RATE: u32 = 75;
/// The master table of contents sits at a fixed place, past the file system area.
const MASTER_TOC_LSN: u64 = 510;
const MASTER_TOC_SECTORS: usize = 10;
/// Sensible ceiling on an area TOC, so a corrupt size field cannot ask for a huge read.
const MAX_AREA_TOC_SECTORS: usize = 96;
/// The only sampling frequency the Scarlet Book defines.
const SAMPLE_FREQUENCY_DSD64: u8 = 4;
const DSD64_HZ: u32 = 2_822_400;
/// Text kinds, of which this player shows two.
const TRACK_TEXT_TITLE: u8 = 0x01;
const TRACK_TEXT_PERFORMER: u8 = 0x02;

/// One track of an audio area.
#[derive(Debug, Clone)]
pub struct Track {
    pub number: u32,
    pub start_lsn: u32,
    pub end_lsn: u32,
    /// Timecode of the track's first audio frame, counted from the start of the area.
    pub start_frame: u32,
    pub frames: u32,
    pub tags: TrackTags,
}

impl Track {
    pub const fn end_frame(&self) -> u32 {
        self.start_frame + self.frames
    }
}

/// One audio area of a disc: the two-channel mix or the multichannel one.
#[derive(Debug, Clone)]
pub struct Area {
    pub channels: u16,
    pub rate: DsdRate,
    pub tracks: Vec<Track>,
}

/// What a SACD image holds, read once when the file is opened.
#[derive(Debug, Clone)]
pub struct Toc {
    pub album: Option<String>,
    pub artist: Option<String>,
    pub area: Area,
}

impl Toc {
    /// Read the table of contents, preferring the two-channel area. A disc with only a
    /// multichannel area plays that instead, on a device with the channels for it.
    pub fn read(file: &mut File) -> Result<Self> {
        let master = read_sectors(file, MASTER_TOC_LSN, MASTER_TOC_SECTORS)?;
        ensure!(
            &master[0..8] == b"SACDMTOC",
            "not a SACD image: no master table of contents at sector {MASTER_TOC_LSN}"
        );
        let (album, artist) = master_text(&master);

        let areas = [
            (u32_at(&master, 64), u16_at(&master, 84)),
            (u32_at(&master, 72), u16_at(&master, 86)),
        ];
        let mut best: Option<Area> = None;
        for (lsn, sectors) in areas {
            if lsn == 0 || sectors == 0 {
                continue;
            }
            let data = read_sectors(
                file,
                u64::from(lsn),
                usize::from(sectors).min(MAX_AREA_TOC_SECTORS),
            )?;
            let Ok(area) = read_area(&data) else {
                continue;
            };
            // Two channels first: it is the mix every DAC can play, and the one a listener
            // asking for a stereo player means.
            if area.channels == 2 || best.is_none() {
                let stereo = area.channels == 2;
                best = Some(area);
                if stereo {
                    break;
                }
            }
        }

        let Some(area) = best else {
            bail!("the image declares no audio area this player can read");
        };
        Ok(Self {
            album,
            artist,
            area,
        })
    }
}

/// The album title and artist, from the master text block.
fn master_text(master: &[u8]) -> (Option<String>, Option<String>) {
    let Some(block) = find_block(master, b"SACDText") else {
        return (None, None);
    };
    // Album first, then the disc's own names where a set gives the disc a different one.
    let title = string_at(block, u16_at(block, 16)).or_else(|| string_at(block, u16_at(block, 32)));
    let artist =
        string_at(block, u16_at(block, 18)).or_else(|| string_at(block, u16_at(block, 34)));
    (title, artist)
}

/// Parse one area TOC and the track list that follows it.
fn read_area(data: &[u8]) -> Result<Area> {
    ensure!(
        &data[0..8] == b"TWOCHTOC" || &data[0..8] == b"MULCHTOC",
        "area does not start with an area table of contents"
    );
    let frequency = data[20];
    ensure!(
        frequency == SAMPLE_FREQUENCY_DSD64,
        "unsupported SACD sampling frequency code {frequency}"
    );
    let channels = u16::from(data[32]);
    ensure!(
        (1..=6).contains(&channels),
        "unsupported channel count {channels}"
    );
    let count = usize::from(data[69]);
    ensure!(count > 0, "the area declares no tracks");

    let mut starts = Vec::new();
    let mut lengths = Vec::new();
    let mut times = Vec::new();
    let mut durations = Vec::new();
    let mut titles = vec![TrackTags::default(); count];
    let mut text_read = false;
    for (index, sector) in data.chunks_exact(SECTOR).enumerate().skip(1) {
        match &sector[0..8] {
            b"SACDTRL1" => {
                for index in 0..count {
                    starts.push(u32_at(sector, 8 + index * 4));
                    lengths.push(u32_at(sector, 1028 + index * 4));
                }
            }
            b"SACDTRL2" => {
                for index in 0..count {
                    times.push(timecode(&sector[8 + index * 4..]));
                    durations.push(timecode(&sector[1028 + index * 4..]));
                }
            }
            // A text block runs past its own sector when the titles are long, so it is read
            // from where it starts to the end of the area. Only the first language is kept.
            b"SACDTTxt" if !text_read => {
                read_track_text(&data[index * SECTOR..], &mut titles);
                text_read = true;
            }
            _ => {}
        }
    }
    ensure!(
        starts.len() == count && times.len() == count,
        "the area is missing its track list"
    );

    let mut tracks = Vec::with_capacity(count);
    for index in 0..count {
        let mut tags = std::mem::take(&mut titles[index]);
        tags.track = Some(index as u32 + 1);
        tracks.push(Track {
            number: index as u32 + 1,
            start_lsn: starts[index],
            end_lsn: starts[index] + lengths[index],
            start_frame: times[index],
            frames: durations[index],
            tags,
        });
    }
    Ok(Area {
        channels,
        rate: DsdRate::new(DSD64_HZ),
        tracks,
    })
}

/// Track titles and performers, each track's text sitting where the block's index says.
fn read_track_text(block: &[u8], tracks: &mut [TrackTags]) {
    for (index, tags) in tracks.iter_mut().enumerate() {
        let position = usize::from(u16_at(block, 8 + index * 2));
        if position == 0 || position >= block.len() {
            continue;
        }
        // A count, three bytes this player has no use for, then that many fields.
        let fields = usize::from(block[position]);
        let mut cursor = position + 4;
        for _ in 0..fields {
            let Some(kind) = block.get(cursor).copied() else {
                return;
            };
            // Each field is its kind, a byte that is always a space, then its text.
            cursor += 2;
            let Some(rest) = block.get(cursor..) else {
                return;
            };
            let length = rest
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(rest.len());
            let text = string(&rest[..length]);
            if kind == TRACK_TEXT_TITLE {
                tags.title = text;
            } else if kind == TRACK_TEXT_PERFORMER {
                tags.artist = text;
            }
            cursor += length;
            while block.get(cursor).copied() == Some(0) {
                cursor += 1;
            }
        }
    }
}

/// Timecodes count minutes, seconds and frames; one number of frames is easier to compare.
fn timecode(bytes: &[u8]) -> u32 {
    u32::from(bytes[0]) * 60 * FRAME_RATE + u32::from(bytes[1]) * FRAME_RATE + u32::from(bytes[2])
}

/// The first sector of `data` whose first eight bytes are `id`.
fn find_block<'a>(data: &'a [u8], id: &[u8; 8]) -> Option<&'a [u8]> {
    data.chunks_exact(SECTOR).find(|sector| &sector[0..8] == id)
}

/// The NUL-terminated string at `offset`, as UTF-8 where it is and Latin-1 where it is not.
/// Discs predate UTF-8 and mostly carry ISO 8859-1, but a modern rip may hold either.
fn string_at(block: &[u8], offset: u16) -> Option<String> {
    let bytes = block.get(usize::from(offset)..)?;
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    string(&bytes[..end])
}

fn string(bytes: &[u8]) -> Option<String> {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        Err(_) => bytes.iter().map(|byte| char::from(*byte)).collect(),
    };
    let text = text.trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_be_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("2 bytes in range"),
    )
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("4 bytes in range"),
    )
}

pub fn read_sectors(file: &mut File, lsn: u64, count: usize) -> Result<Vec<u8>> {
    let mut data = vec![0_u8; count * SECTOR];
    file.seek(SeekFrom::Start(lsn * SECTOR as u64))?;
    file.read_exact(&mut data)?;
    Ok(data)
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::reader::sacd::toc::{SECTOR, read_area, string_at, timecode};
    use crate::reader::tags::TrackTags;

    /// An area TOC and the track list sectors that follow it, as a disc lays them out.
    pub(crate) fn area_toc(tracks: &[(u32, u32, u32, u32)], titles: &[&str]) -> Vec<u8> {
        let mut data = vec![0_u8; SECTOR * 4];
        data[0..8].copy_from_slice(b"TWOCHTOC");
        data[20] = 4;
        data[32] = 2;
        data[69] = tracks.len() as u8;

        let list = &mut data[SECTOR..SECTOR * 2];
        list[0..8].copy_from_slice(b"SACDTRL1");
        for (index, (start, length, _, _)) in tracks.iter().enumerate() {
            list[8 + index * 4..12 + index * 4].copy_from_slice(&start.to_be_bytes());
            list[1028 + index * 4..1032 + index * 4].copy_from_slice(&length.to_be_bytes());
        }

        let times = &mut data[SECTOR * 2..SECTOR * 3];
        times[0..8].copy_from_slice(b"SACDTRL2");
        for (index, (_, _, start, frames)) in tracks.iter().enumerate() {
            let at = 8 + index * 4;
            times[at] = (start / (60 * 75)) as u8;
            times[at + 1] = ((start / 75) % 60) as u8;
            times[at + 2] = (start % 75) as u8;
            let at = 1028 + index * 4;
            times[at] = (frames / (60 * 75)) as u8;
            times[at + 1] = ((frames / 75) % 60) as u8;
            times[at + 2] = (frames % 75) as u8;
        }

        let text = &mut data[SECTOR * 3..SECTOR * 4];
        text[0..8].copy_from_slice(b"SACDTTxt");
        let mut cursor = 8 + titles.len() * 2 + 8;
        for (index, title) in titles.iter().enumerate() {
            let position = cursor as u16;
            text[8 + index * 2..10 + index * 2].copy_from_slice(&position.to_be_bytes());
            text[cursor] = 1;
            cursor += 4;
            text[cursor] = 1;
            text[cursor + 1] = 0x20;
            cursor += 2;
            text[cursor..cursor + title.len()].copy_from_slice(title.as_bytes());
            cursor += title.len() + 1;
        }
        data
    }

    #[test]
    fn a_timecode_counts_frames_from_the_start_of_the_area() {
        assert_eq!(timecode(&[0, 0, 0]), 0);
        assert_eq!(timecode(&[1, 2, 3]), 75 * 62 + 3);
    }

    #[test]
    fn the_track_list_gives_every_track_its_sectors_its_place_and_its_title() {
        let data = area_toc(
            &[(584, 100, 0, 150), (684, 200, 150, 300)],
            &["Allegro moderato", "Adagio"],
        );

        let area = read_area(&data).expect("parses");

        assert_eq!(area.channels, 2);
        assert_eq!(area.rate.hz(), 2_822_400);
        assert_eq!(area.tracks.len(), 2);
        assert_eq!(area.tracks[0].start_lsn, 584);
        assert_eq!(area.tracks[0].end_lsn, 684);
        assert_eq!(area.tracks[1].start_frame, 150);
        assert_eq!(area.tracks[1].frames, 300);
        assert_eq!(area.tracks[1].end_frame(), 450);
        assert_eq!(
            area.tracks[0].tags.title.as_deref(),
            Some("Allegro moderato")
        );
        assert_eq!(area.tracks[1].tags.title.as_deref(), Some("Adagio"));
        assert_eq!(area.tracks[1].tags.track, Some(2));
    }

    #[test]
    fn an_area_with_no_track_list_is_refused_rather_than_played_as_one_long_track() {
        let mut data = area_toc(&[(584, 100, 0, 150)], &["Only"]);
        data[SECTOR..SECTOR + 8].copy_from_slice(b"XXXXXXXX");

        let error = read_area(&data).expect_err("refused");

        assert!(
            error.to_string().contains("missing its track list"),
            "{error}"
        );
    }

    #[test]
    fn latin_one_text_survives_and_an_empty_field_reads_as_no_text() {
        let mut block = vec![0_u8; 32];
        block[0] = 0xC9;
        block[1] = b't';
        block[2] = b'e';

        assert_eq!(string_at(&block, 0).as_deref(), Some("Éte"));
        assert_eq!(string_at(&block, 8), None);
        assert_eq!(string_at(&block, 99), None);
    }

    #[test]
    fn an_untitled_track_keeps_its_number_and_nothing_else() {
        let data = area_toc(&[(584, 100, 0, 150)], &[""]);

        let area = read_area(&data).expect("parses");

        assert_eq!(
            area.tracks[0].tags,
            TrackTags {
                track: Some(1),
                ..TrackTags::default()
            }
        );
    }
}
