//! One track of a file that holds a whole side, bounded by the times a cue sheet gives.
//!
//! The bounds wrap the source rather than the [`crate::reader::Source`] around it, so the
//! native DSD path, which takes the [`DsdSource`] out and carries the bytes itself, stays
//! inside the track too.

use anyhow::Result;

use crate::audio::PcmFormat;
use crate::dsd::DsdFormat;
use crate::reader::tags::TrackTags;
use crate::reader::{DsdSource, PcmSource};

/// What a read of the wrapped source is worth once the bounds are applied.
enum Take {
    /// Entirely before the start of the track, so nothing of it is played.
    Drop,
    Keep {
        skip: usize,
        kept: usize,
    },
}

/// Where inside the wrapped source the track sits, in whatever unit that source counts in:
/// DSD bytes per channel, or PCM frames.
///
/// A container addressable only in whole blocks lands a seek short of the bound, so what the
/// block carries from before the track starts is read and dropped rather than played.
struct Window {
    start: u64,
    length: u64,
    position: u64,
    skip: u64,
}

impl Window {
    const fn new(start: u64, end: u64) -> Self {
        Self {
            start,
            length: end.saturating_sub(start),
            position: 0,
            skip: 0,
        }
    }

    const fn remaining(&self) -> u64 {
        self.length.saturating_sub(self.position)
    }

    /// Record a seek that asked for `offset` into the track and landed at `landed` in the
    /// file, and report where that leaves the track's own position.
    const fn landed(&mut self, offset: u64, landed: u64) -> u64 {
        let target = self.start + offset;
        self.skip = target.saturating_sub(landed);
        self.position = if landed > target {
            landed - self.start
        } else {
            offset
        };
        self.position
    }

    /// How much of a read of `count` belongs to the track, dropping what the seek landed
    /// short by and stopping at the end of the track.
    fn take(&mut self, count: usize) -> Take {
        if self.skip >= count as u64 {
            self.skip -= count as u64;
            return Take::Drop;
        }
        let skip = self.skip as usize;
        self.skip = 0;
        let kept = ((count - skip) as u64).min(self.remaining()) as usize;
        self.position += kept as u64;
        Take::Keep { skip, kept }
    }
}

/// One cue-sheet track of a DSD file.
pub struct SpanDsd {
    inner: Box<dyn DsdSource>,
    window: Window,
    tags: TrackTags,
}

impl SpanDsd {
    pub fn new(inner: Box<dyn DsdSource>, start: u64, end: u64, tags: TrackTags) -> Result<Self> {
        let mut span = Self {
            inner,
            window: Window::new(start, end),
            tags,
        };
        span.seek(0)?;
        Ok(span)
    }
}

impl DsdSource for SpanDsd {
    fn container(&self) -> &'static str {
        self.inner.container()
    }

    fn format(&self) -> DsdFormat {
        self.inner.format()
    }

    fn total_bytes_per_channel(&self) -> u64 {
        self.window.length
    }

    fn tags(&self) -> &TrackTags {
        &self.tags
    }

    fn chunk_bytes(&self) -> usize {
        self.inner.chunk_bytes()
    }

    fn read(&mut self, planes: &mut [Box<[u8]>]) -> Result<usize> {
        if self.window.remaining() == 0 {
            return Ok(0);
        }
        loop {
            let count = self.inner.read(planes)?;
            if count == 0 {
                return Ok(0);
            }
            let Take::Keep { skip, kept } = self.window.take(count) else {
                continue;
            };
            if skip > 0 {
                for plane in planes.iter_mut() {
                    plane.copy_within(skip..skip + kept, 0);
                }
            }
            return Ok(kept);
        }
    }

    fn seek(&mut self, bytes_per_channel: u64) -> Result<u64> {
        let offset = bytes_per_channel.min(self.window.length);
        let landed = self.inner.seek(self.window.start + offset)?;
        Ok(self.window.landed(offset, landed))
    }
}

/// One cue-sheet track of a PCM file.
pub struct SpanPcm {
    inner: Box<dyn PcmSource>,
    window: Window,
    tags: TrackTags,
    channels: usize,
    /// The block the wrapped source decoded, before the part outside the track is dropped.
    block: Vec<i32>,
}

impl SpanPcm {
    pub fn new(inner: Box<dyn PcmSource>, start: u64, end: u64, tags: TrackTags) -> Result<Self> {
        let channels = inner.format().channels as usize;
        let mut span = Self {
            inner,
            window: Window::new(start, end),
            tags,
            channels,
            block: Vec::new(),
        };
        span.seek(0)?;
        Ok(span)
    }
}

impl PcmSource for SpanPcm {
    fn container(&self) -> &'static str {
        self.inner.container()
    }

    fn format(&self) -> PcmFormat {
        self.inner.format()
    }

    fn total_frames(&self) -> u64 {
        self.window.length
    }

    fn tags(&self) -> &TrackTags {
        &self.tags
    }

    fn chunk_frames(&self) -> usize {
        self.inner.chunk_frames()
    }

    fn read(&mut self, out: &mut Vec<i32>) -> Result<usize> {
        if self.window.remaining() == 0 {
            return Ok(0);
        }
        loop {
            self.block.clear();
            let count = self.inner.read(&mut self.block)?;
            if count == 0 {
                return Ok(0);
            }
            let Take::Keep { skip, kept } = self.window.take(count) else {
                continue;
            };
            let start = skip * self.channels;
            out.extend_from_slice(&self.block[start..start + kept * self.channels]);
            return Ok(kept);
        }
    }

    fn seek(&mut self, frame: u64) -> Result<u64> {
        let offset = frame.min(self.window.length);
        let landed = self.inner.seek(self.window.start + offset)?;
        self.block.clear();
        Ok(self.window.landed(offset, landed))
    }
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use crate::audio::PcmFormat;
    use crate::dsd::{DsdFormat, DsdRate};
    use crate::reader::span::{SpanDsd, SpanPcm, Window};
    use crate::reader::tags::TrackTags;
    use crate::reader::{DsdSource, PcmSource};

    /// A DSD source of ascending bytes, addressable only in whole blocks the way DSF is.
    struct Blocks {
        bytes: Vec<u8>,
        block: usize,
        position: usize,
    }

    impl DsdSource for Blocks {
        fn container(&self) -> &'static str {
            "test"
        }

        fn format(&self) -> DsdFormat {
            DsdFormat {
                rate: DsdRate::new(2_822_400),
                channels: 1,
            }
        }

        fn total_bytes_per_channel(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn tags(&self) -> &TrackTags {
            const EMPTY: &TrackTags = &TrackTags {
                title: None,
                artist: None,
                album: None,
                track: None,
            };
            EMPTY
        }

        fn chunk_bytes(&self) -> usize {
            self.block
        }

        fn read(&mut self, planes: &mut [Box<[u8]>]) -> Result<usize> {
            let end = (self.position + self.block).min(self.bytes.len());
            let count = end - self.position;
            planes[0][..count].copy_from_slice(&self.bytes[self.position..end]);
            self.position = end;
            Ok(count)
        }

        fn seek(&mut self, bytes_per_channel: u64) -> Result<u64> {
            let block = bytes_per_channel.min(self.bytes.len() as u64) / self.block as u64;
            self.position = block as usize * self.block;
            Ok(self.position as u64)
        }
    }

    fn blocks() -> Box<dyn DsdSource> {
        Box::new(Blocks {
            bytes: (0..=255).collect(),
            block: 16,
            position: 0,
        })
    }

    fn drain_dsd(source: &mut dyn DsdSource) -> Vec<u8> {
        let mut planes = vec![vec![0_u8; source.chunk_bytes()].into_boxed_slice()];
        let mut out = Vec::new();
        loop {
            let count = source.read(&mut planes).expect("reads");
            if count == 0 {
                return out;
            }
            out.extend_from_slice(&planes[0][..count]);
        }
    }

    #[test]
    fn a_span_plays_only_its_own_bytes_although_the_seek_lands_on_a_block() {
        let mut span = SpanDsd::new(blocks(), 20, 50, TrackTags::default()).expect("opens");

        assert_eq!(span.total_bytes_per_channel(), 30);
        assert_eq!(drain_dsd(&mut span), (20_u8..50).collect::<Vec<u8>>());
    }

    #[test]
    fn a_span_seek_counts_from_the_start_of_the_track() {
        let mut span = SpanDsd::new(blocks(), 20, 50, TrackTags::default()).expect("opens");

        assert_eq!(span.seek(5).expect("seeks"), 5);
        assert_eq!(drain_dsd(&mut span), (25_u8..50).collect::<Vec<u8>>());
    }

    #[test]
    fn a_span_seek_past_its_end_lands_on_its_end_and_reads_nothing_more() {
        let mut span = SpanDsd::new(blocks(), 20, 50, TrackTags::default()).expect("opens");

        assert_eq!(span.seek(999).expect("seeks"), 30);
        assert!(drain_dsd(&mut span).is_empty());
    }

    #[test]
    fn the_last_span_of_a_file_runs_to_the_end_of_it() {
        let mut span = SpanDsd::new(blocks(), 240, 256, TrackTags::default()).expect("opens");

        assert_eq!(drain_dsd(&mut span), (240_u8..=255).collect::<Vec<u8>>());
    }

    #[test]
    fn a_span_reports_the_cue_sheets_names_rather_than_the_files() {
        let tags = TrackTags {
            title: Some("So What".to_owned()),
            ..TrackTags::default()
        };

        let span = SpanDsd::new(blocks(), 0, 16, tags).expect("opens");

        assert_eq!(span.tags().title.as_deref(), Some("So What"));
        assert_eq!(span.container(), "test");
    }

    /// A PCM source of ascending frames, addressable to the frame.
    struct Frames {
        frames: u64,
        position: u64,
        block: usize,
    }

    impl PcmSource for Frames {
        fn container(&self) -> &'static str {
            "test"
        }

        fn format(&self) -> PcmFormat {
            PcmFormat {
                rate: 44_100,
                bits: 16,
                channels: 2,
            }
        }

        fn total_frames(&self) -> u64 {
            self.frames
        }

        fn tags(&self) -> &TrackTags {
            const EMPTY: &TrackTags = &TrackTags {
                title: None,
                artist: None,
                album: None,
                track: None,
            };
            EMPTY
        }

        fn chunk_frames(&self) -> usize {
            self.block
        }

        fn read(&mut self, out: &mut Vec<i32>) -> Result<usize> {
            let end = (self.position + self.block as u64).min(self.frames);
            for frame in self.position..end {
                out.push(frame as i32);
                out.push(-(frame as i32));
            }
            let count = end - self.position;
            self.position = end;
            Ok(count as usize)
        }

        fn seek(&mut self, frame: u64) -> Result<u64> {
            self.position = frame.min(self.frames);
            Ok(self.position)
        }
    }

    #[test]
    fn a_pcm_span_plays_only_the_frames_between_its_bounds() {
        let inner = Box::new(Frames {
            frames: 100,
            position: 0,
            block: 7,
        });
        let mut span = SpanPcm::new(inner, 10, 25, TrackTags::default()).expect("opens");

        let mut out = Vec::new();
        while span.read(&mut out).expect("reads") > 0 {}

        assert_eq!(span.total_frames(), 15);
        let left: Vec<i32> = out.chunks_exact(2).map(|frame| frame[0]).collect();
        assert_eq!(left, (10..25).collect::<Vec<i32>>());
    }

    #[test]
    fn a_window_that_lands_past_its_target_owes_no_skip() {
        let mut window = Window::new(10, 20);

        assert_eq!(window.landed(0, 12), 2);
        assert_eq!(window.skip, 0);
        assert_eq!(window.remaining(), 8);
    }
}
