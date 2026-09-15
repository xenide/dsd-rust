//! SACD disc images: the Scarlet Book layout, and the DSD inside it.
//!
//! An image holds a whole disc, so it opens as a list of tracks rather than as one file. The
//! audio sits in 2048-byte sectors carrying packets, which carry audio frames of a
//! seventy-fifth of a second each, either as plain DSD or compressed with DST.

pub mod dst;
pub mod toc;

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::dsd::{DSD_SILENCE_BYTE, DsdFormat};
use crate::reader::DsdSource;
use crate::reader::sacd::toc::{FRAME_RATE, SECTOR, Toc, Track};
use crate::reader::tags::TrackTags;

/// DSD bits one audio frame carries per channel: a seventy-fifth of a second.
const SAMPLES_PER_FRAME: usize = 588 * 64;
const FRAME_BYTES: usize = SAMPLES_PER_FRAME / 8;
/// The packet kinds a sector can carry; only audio is worth reading.
const DATA_TYPE_AUDIO: u8 = 2;
/// A sector describes at most this many packets and frame starts.
const MAX_PACKETS: usize = 7;
/// How far a seek probe looks for a sector that starts an audio frame before giving up and
/// landing earlier, which costs demuxing rather than correctness.
const PROBE_SECTORS: u32 = 64;
/// Read buffer over the image, sized so one refill covers several audio frames.
const READ_BUFFER: usize = 64 * SECTOR;

/// A disc image, opened to see what it holds before anything is played.
pub struct Disc {
    path: std::path::PathBuf,
    toc: Toc,
}

impl Disc {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file =
            File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let toc =
            Toc::read(&mut file).with_context(|| format!("cannot read {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            toc,
        })
    }

    pub fn tracks(&self) -> &[Track] {
        &self.toc.area.tracks
    }

    /// What a track is called, with the album's own names filled in around it.
    pub fn tags(&self, number: u32) -> TrackTags {
        let mut tags = match self.track(number) {
            Some(track) => track.tags.clone(),
            None => TrackTags::default(),
        };
        tags.album = self.toc.album.clone();
        if tags.artist.is_none() {
            tags.artist = self.toc.artist.clone();
        }
        tags
    }

    fn track(&self, number: u32) -> Option<&Track> {
        self.tracks().iter().find(|track| track.number == number)
    }

    /// Open one track for playback. Track numbers are what the disc calls them, from one.
    pub fn reader(&self, number: u32) -> Result<SacdReader> {
        let Some(track) = self.track(number).cloned() else {
            bail!(
                "{} holds tracks 1 to {}, so there is no track {number}",
                self.path.display(),
                self.tracks().len()
            );
        };
        let channels = self.toc.area.channels;
        let file = File::open(&self.path)?;
        let mut reader = SacdReader {
            file: BufReader::with_capacity(READ_BUFFER, file),
            format: DsdFormat {
                rate: self.toc.area.rate,
                channels,
            },
            tags: self.tags(number),
            demux: Demux::new(channels as usize, track.start_lsn, track.end_lsn),
            decoder: dst::Decoder::new(channels as usize, SAMPLES_PER_FRAME)?,
            pending: None,
            emitted: 0,
            track,
        };
        // The track's first sector may open with the tail of the one before it.
        let start = reader.track.start_frame;
        reader.skip_to(start)?;
        Ok(reader)
    }
}

/// One audio frame, as the sectors carried it.
struct Frame {
    /// Frames since the start of the area, which is what track boundaries are given in.
    timecode: u32,
    dst_encoded: bool,
    data: Vec<u8>,
}

/// Pulls audio frames out of the sectors of one track.
struct Demux {
    channels: usize,
    /// The next sector to parse, and the one past the end of the track.
    sector: u32,
    end: u32,
    /// Where the file is positioned, so consecutive sectors read without a seek.
    cursor: Option<u32>,
    buffer: Vec<u8>,
    /// The frame being assembled, and how many more packets it is waiting for.
    payload: Vec<u8>,
    packets_left: i32,
    started: bool,
    dst_encoded: bool,
    timecode: u32,
    ready: VecDeque<Frame>,
}

impl Demux {
    fn new(channels: usize, start: u32, end: u32) -> Self {
        Self {
            channels,
            sector: start,
            end,
            cursor: None,
            buffer: vec![0; SECTOR],
            payload: Vec::with_capacity(FRAME_BYTES * 2),
            packets_left: 0,
            started: false,
            dst_encoded: false,
            timecode: 0,
            ready: VecDeque::new(),
        }
    }

    /// Start again at `sector`, dropping the frame that was being assembled.
    fn restart(&mut self, sector: u32) {
        self.sector = sector;
        self.started = false;
        self.payload.clear();
        self.ready.clear();
    }

    fn next(&mut self, file: &mut BufReader<File>) -> Result<Option<Frame>> {
        loop {
            if let Some(frame) = self.ready.pop_front() {
                return Ok(Some(frame));
            }
            if self.sector >= self.end {
                return Ok(None);
            }
            self.read_sector(file)?;
        }
    }

    fn read_sector(&mut self, file: &mut BufReader<File>) -> Result<()> {
        if self.cursor != Some(self.sector) {
            file.seek(SeekFrom::Start(u64::from(self.sector) * SECTOR as u64))?;
        }
        let mut buffer = std::mem::take(&mut self.buffer);
        let read = file.read_exact(&mut buffer);
        self.cursor = Some(self.sector + 1);
        self.sector += 1;
        read?;
        self.parse(&buffer);
        self.buffer = buffer;
        Ok(())
    }

    /// Split one sector into packets and hand any frame they complete to the ready queue.
    /// A sector that does not describe audio is skipped rather than refused: the areas a
    /// track spans are the disc's own, and a reader that stopped at the first odd one would
    /// drop the rest of the track.
    fn parse(&mut self, sector: &[u8]) {
        let packets = usize::from(sector[0] >> 5);
        let frame_infos = usize::from((sector[0] >> 2) & 7);
        let dst_encoded = sector[0] & 1 == 1;
        if packets == 0 || packets > MAX_PACKETS || frame_infos > MAX_PACKETS {
            return;
        }
        let info_bytes = if dst_encoded { 4 } else { 3 };
        let info_base = 1 + packets * 2;
        let mut cursor = info_base + frame_infos * info_bytes;
        let mut info = 0;

        for index in 0..packets {
            let head = sector[1 + index * 2];
            let length = (usize::from(head & 7) << 8) | usize::from(sector[2 + index * 2]);
            if cursor + length > SECTOR {
                self.started = false;
                return;
            }
            if (head >> 3) & 7 == DATA_TYPE_AUDIO {
                if head & 0x80 != 0 {
                    if info >= frame_infos {
                        self.started = false;
                        return;
                    }
                    let at = info_base + info * info_bytes;
                    self.open_frame(&sector[at..at + info_bytes], dst_encoded);
                    info += 1;
                }
                self.append(&sector[cursor..cursor + length]);
            }
            cursor += length;
        }
    }

    fn open_frame(&mut self, info: &[u8], dst_encoded: bool) {
        self.timecode = u32::from(info[0]) * 60 * FRAME_RATE
            + u32::from(info[1]) * FRAME_RATE
            + u32::from(info[2]);
        self.dst_encoded = dst_encoded;
        self.packets_left = if dst_encoded {
            i32::from((info[3] >> 2) & 0x1F)
        } else {
            0
        };
        self.payload.clear();
        self.started = true;
    }

    /// Add one packet to the frame being assembled, and publish the frame once it is whole.
    fn append(&mut self, packet: &[u8]) {
        if !self.started {
            return;
        }
        self.payload.extend_from_slice(packet);
        self.packets_left -= 1;
        let complete = if self.dst_encoded {
            self.packets_left == 0
        } else {
            self.payload.len() >= self.channels * FRAME_BYTES
        };
        if !complete {
            return;
        }
        self.ready.push_back(Frame {
            timecode: self.timecode,
            dst_encoded: self.dst_encoded,
            data: std::mem::take(&mut self.payload),
        });
        self.started = false;
    }
}

/// One track of a disc image, read as planar MSB-first DSD like any other container.
pub struct SacdReader {
    file: BufReader<File>,
    format: DsdFormat,
    tags: TrackTags,
    track: Track,
    demux: Demux,
    decoder: dst::Decoder,
    /// A frame a seek looked at and put back.
    pending: Option<Frame>,
    /// Audio frames handed out since the start of the track.
    emitted: u32,
}

impl SacdReader {
    fn next_frame(&mut self) -> Result<Option<Frame>> {
        if let Some(frame) = self.pending.take() {
            return Ok(Some(frame));
        }
        self.demux.next(&mut self.file)
    }

    /// Demux past everything before `timecode`, leaving the first frame at or after it ready
    /// to be read. Nothing is decoded, so this costs sector reads and no more.
    fn skip_to(&mut self, timecode: u32) -> Result<()> {
        while let Some(frame) = self.next_frame()? {
            if frame.timecode >= timecode {
                self.emitted = frame.timecode.saturating_sub(self.track.start_frame);
                self.pending = Some(frame);
                return Ok(());
            }
        }
        self.emitted = self.track.frames;
        Ok(())
    }

    /// The first sector whose next audio frame starts at or after `timecode`.
    ///
    /// Timecodes only rise along a track, so the sectors can be halved rather than walked:
    /// a track is hundreds of thousands of sectors, and a seek reads about twenty of them.
    fn locate(&mut self, timecode: u32) -> Result<u32> {
        let mut low = self.track.start_lsn;
        let mut high = self.track.end_lsn;
        while low < high {
            let middle = low + (high - low) / 2;
            match self.frame_start_from(middle)? {
                // Landing early only costs demuxing, so an unreadable stretch looks backwards.
                Some(found) if found < timecode => low = middle + 1,
                _ => high = middle,
            }
        }
        Ok(low)
    }

    /// The timecode of the first audio frame starting at or after `sector`.
    fn frame_start_from(&mut self, sector: u32) -> Result<Option<u32>> {
        // A header, up to seven packet descriptions, and the first frame info behind them.
        let mut header = [0_u8; 1 + MAX_PACKETS * 2 + 4];
        let limit = sector.saturating_add(PROBE_SECTORS).min(self.track.end_lsn);
        for probe in sector..limit {
            self.file
                .seek(SeekFrom::Start(u64::from(probe) * SECTOR as u64))?;
            self.file.read_exact(&mut header)?;
            self.demux.cursor = None;
            let packets = usize::from(header[0] >> 5);
            if packets == 0 || packets > MAX_PACKETS {
                continue;
            }
            let starts = (0..packets).any(|index| {
                let head = header[1 + index * 2];
                head & 0x80 != 0 && (head >> 3) & 7 == DATA_TYPE_AUDIO
            });
            if !starts {
                continue;
            }
            let at = 1 + packets * 2;
            return Ok(Some(
                u32::from(header[at]) * 60 * FRAME_RATE
                    + u32::from(header[at + 1]) * FRAME_RATE
                    + u32::from(header[at + 2]),
            ));
        }
        Ok(None)
    }

    /// Spread one frame of plain DSD, which arrives one byte per channel in turn.
    fn deinterleave(&self, frame: &[u8], planes: &mut [Box<[u8]>]) {
        for (channel, plane) in planes.iter_mut().enumerate().take(self.channels()) {
            for index in 0..FRAME_BYTES {
                plane[index] = frame
                    .get(index * self.channels() + channel)
                    .copied()
                    .unwrap_or(DSD_SILENCE_BYTE);
            }
        }
    }

    const fn channels(&self) -> usize {
        self.format.channels as usize
    }
}

impl DsdSource for SacdReader {
    fn container(&self) -> &'static str {
        "SACD"
    }

    fn format(&self) -> DsdFormat {
        self.format
    }

    fn total_bytes_per_channel(&self) -> u64 {
        u64::from(self.track.frames) * FRAME_BYTES as u64
    }

    fn tags(&self) -> &TrackTags {
        &self.tags
    }

    fn chunk_bytes(&self) -> usize {
        FRAME_BYTES
    }

    fn read(&mut self, planes: &mut [Box<[u8]>]) -> Result<usize> {
        let Some(frame) = self.next_frame()? else {
            return Ok(0);
        };
        if frame.timecode >= self.track.end_frame() {
            return Ok(0);
        }
        self.emitted = frame.timecode.saturating_sub(self.track.start_frame) + 1;
        if frame.dst_encoded {
            self.decoder.decode(&frame.data, planes).with_context(|| {
                format!("track {} at frame {}", self.track.number, frame.timecode)
            })?;
        } else {
            self.deinterleave(&frame.data, planes);
        }
        Ok(FRAME_BYTES)
    }

    /// A disc is addressable by audio frame, so a seek lands on the frame holding the target.
    fn seek(&mut self, bytes_per_channel: u64) -> Result<u64> {
        let index = (bytes_per_channel / FRAME_BYTES as u64).min(u64::from(self.track.frames));
        let timecode = self.track.start_frame + index as u32;
        let sector = self.locate(timecode)?;
        self.pending = None;
        self.demux.restart(sector);
        self.skip_to(timecode)?;
        Ok(u64::from(self.emitted) * FRAME_BYTES as u64)
    }
}

/// True when `path` names a file this reader can open.
pub fn is_image(path: &Path) -> bool {
    let Some(extension) = path.extension() else {
        return false;
    };
    extension.eq_ignore_ascii_case("iso")
}

#[cfg(test)]
mod tests {
    use crate::reader::sacd::toc::SECTOR;
    use crate::reader::sacd::{Demux, FRAME_BYTES};

    /// One audio sector: a header, packet descriptions, frame info, then packet payloads.
    fn sector(packets: &[(bool, usize)], timecode: u32, sector_count: u8, fill: u8) -> Vec<u8> {
        let starts = packets.iter().filter(|(start, _)| *start).count();
        let mut data = vec![0_u8; SECTOR];
        data[0] = ((packets.len() as u8) << 5) | ((starts as u8) << 2) | 1;
        let info_base = 1 + packets.len() * 2;
        let mut cursor = info_base + starts * 4;
        let mut info = 0;
        for (index, (start, length)) in packets.iter().enumerate() {
            let head = (u8::from(*start) << 7) | (2 << 3) | ((length >> 8) as u8 & 7);
            data[1 + index * 2] = head;
            data[2 + index * 2] = (length & 0xFF) as u8;
            if *start {
                let at = info_base + info * 4;
                data[at] = (timecode / (60 * 75)) as u8;
                data[at + 1] = ((timecode / 75) % 60) as u8;
                data[at + 2] = (timecode % 75) as u8;
                data[at + 3] = sector_count << 2;
                info += 1;
            }
            data[cursor..cursor + length].fill(fill);
            cursor += length;
        }
        data
    }

    fn demux_over(sectors: Vec<Vec<u8>>) -> Demux {
        let mut demux = Demux::new(2, 0, sectors.len() as u32);
        for data in sectors {
            demux.sector += 1;
            demux.parse(&data);
        }
        demux
    }

    #[test]
    fn a_frame_spanning_two_sectors_comes_out_whole_and_in_order() {
        let mut demux = demux_over(vec![
            sector(&[(true, 100)], 0, 2, 0xAA),
            sector(&[(false, 50), (true, 60)], 1, 1, 0xBB),
        ]);

        let first = demux.ready.pop_front().expect("first frame");
        assert_eq!(first.timecode, 0);
        assert_eq!(first.data.len(), 150);
        assert_eq!(first.data[0], 0xAA);
        assert_eq!(first.data[149], 0xBB);

        let second = demux.ready.pop_front().expect("second frame");
        assert_eq!(second.timecode, 1);
        assert_eq!(second.data.len(), 60);
    }

    #[test]
    fn a_sector_that_describes_no_audio_is_skipped_rather_than_ending_the_track() {
        let mut blank = vec![0_u8; SECTOR];
        blank[0] = 0;
        let demux = demux_over(vec![blank, sector(&[(true, 40)], 7, 1, 0xCC)]);

        assert_eq!(demux.ready.len(), 1);
        assert_eq!(demux.ready[0].timecode, 7);
    }

    #[test]
    fn a_packet_claiming_more_bytes_than_a_sector_holds_drops_the_frame() {
        let demux = demux_over(vec![sector(&[(true, 40)], 0, 1, 1), {
            let mut bad = vec![0_u8; SECTOR];
            bad[0] = (1 << 5) | (1 << 2) | 1;
            bad[1] = 0x80 | (2 << 3) | 7;
            bad[2] = 0xFF;
            bad
        }]);

        assert_eq!(demux.ready.len(), 1);
    }

    #[test]
    fn an_uncompressed_frame_completes_on_its_own_length() {
        let mut demux = Demux::new(2, 0, 1);
        demux.channels = 2;
        demux.open_frame(&[0, 0, 5], false);
        demux.append(&vec![0x69; FRAME_BYTES]);
        assert!(demux.ready.is_empty());
        demux.append(&vec![0x69; FRAME_BYTES]);

        assert_eq!(demux.ready.len(), 1);
        assert_eq!(demux.ready[0].data.len(), FRAME_BYTES * 2);
        assert!(!demux.ready[0].dst_encoded);
    }
}
