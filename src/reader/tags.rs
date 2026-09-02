use std::io::{Read, Seek, SeekFrom};

use anyhow::Result;

use crate::reader::read_available;

const ID3_HEADER_LEN: usize = 10;
/// A tag larger than this is carrying artwork this player has no use for, so stop reading.
const MAX_TAG_BYTES: usize = 4 << 20;

/// What a container says the recording is, as far as this player displays it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TrackTags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub track: Option<u32>,
}

impl TrackTags {
    /// One line naming the recording, or whichever half of it the file carries.
    pub fn headline(&self) -> Option<String> {
        match (&self.artist, &self.title) {
            (Some(artist), Some(title)) => Some(format!("{artist} — {title}")),
            (None, Some(title)) => Some(title.clone()),
            (Some(artist), None) => Some(artist.clone()),
            (None, None) => None,
        }
    }

    /// A file-pane label: the track number and title, for a pane with no room for more.
    pub fn label(&self) -> Option<String> {
        let title = self.title.as_ref()?;
        match self.track {
            Some(number) => Some(format!("{number:>2}. {title}")),
            None => Some(title.clone()),
        }
    }
}

/// Read the ID3v2 tag at `offset`. A malformed tag is not worth refusing a playable file
/// over, so anything unparseable comes back as no tags rather than an error.
pub fn read_id3v2<R: Read + Seek>(inner: &mut R, offset: u64) -> Result<TrackTags> {
    inner.seek(SeekFrom::Start(offset))?;
    let mut header = [0_u8; ID3_HEADER_LEN];
    if read_available(inner, &mut header)? != ID3_HEADER_LEN || &header[0..3] != b"ID3" {
        return Ok(TrackTags::default());
    }
    let declared = syncsafe(&header[6..10]) as usize;
    let mut body = vec![0_u8; declared.min(MAX_TAG_BYTES)];
    let filled = read_available(inner, &mut body)?;
    body.truncate(filled);
    Ok(parse_id3v2(header[3], header[5], body))
}

/// Read the artist and title out of a DSDIFF edited-master information chunk.
pub fn parse_diin(body: &[u8], tags: &mut TrackTags) {
    let mut offset = 0;
    while offset + 12 <= body.len() {
        let id = &body[offset..offset + 4];
        let size = u64::from_be_bytes(body[offset + 4..offset + 12].try_into().expect("8 bytes"));
        let start = offset + 12;
        let Some(end) = start
            .checked_add(size as usize)
            .filter(|end| *end <= body.len())
        else {
            return;
        };
        // Both text sub-chunks are a 32-bit count followed by that many characters.
        if size >= 4 {
            let text = ascii_text(&body[start + 4..end]);
            match id {
                b"DIAR" => tags.artist = text,
                b"DITI" => tags.title = text,
                _ => {}
            }
        }
        offset = end + end % 2;
    }
}

fn ascii_text(bytes: &[u8]) -> Option<String> {
    let mut text = String::with_capacity(bytes.len());
    for byte in bytes {
        if *byte == 0 {
            break;
        }
        text.push(char::from(*byte));
    }
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

/// The four text frames this player shows. Everything else in a tag is skipped.
enum Field {
    Title,
    Artist,
    Album,
    Track,
}

/// ID3v2.2 named its frames in three characters; 2.3 and 2.4 use four.
fn field_of(id: &[u8]) -> Option<Field> {
    match id {
        b"TIT2" | b"TT2" => Some(Field::Title),
        b"TPE1" | b"TP1" => Some(Field::Artist),
        b"TALB" | b"TAL" => Some(Field::Album),
        b"TRCK" | b"TRK" => Some(Field::Track),
        _ => None,
    }
}

fn syncsafe(bytes: &[u8]) -> u32 {
    let mut value = 0;
    for byte in bytes {
        value = value << 7 | u32::from(byte & 0x7F);
    }
    value
}

fn parse_id3v2(version: u8, flags: u8, body: Vec<u8>) -> TrackTags {
    let mut tags = TrackTags::default();
    if !(2..=4).contains(&version) {
        return tags;
    }
    // Unsynchronisation hides 0xFF 0x00 pairs in the tag so a decoder never mistakes one for
    // an MPEG frame sync. Undo it before the frame headers are read, or every size is wrong.
    let body = if flags & 0x80 == 0 {
        body
    } else {
        desynchronise(&body)
    };
    let start = if flags & 0x40 == 0 {
        0
    } else {
        extended_header_len(version, &body)
    };
    let Some(frames) = body.get(start..) else {
        return tags;
    };
    collect_frames(version, frames, &mut tags);
    tags
}

fn desynchronise(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut index = 0;
    while index < body.len() {
        out.push(body[index]);
        if body[index] == 0xFF && body.get(index + 1) == Some(&0) {
            index += 1;
        }
        index += 1;
    }
    out
}

/// 2.3 sizes the extended header without counting its own size field; 2.4 counts it.
fn extended_header_len(version: u8, body: &[u8]) -> usize {
    let Some(size) = body.get(0..4) else {
        return body.len();
    };
    match version {
        4 => syncsafe(size) as usize,
        _ => u32::from_be_bytes(size.try_into().expect("4 bytes")) as usize + 4,
    }
}

fn collect_frames(version: u8, body: &[u8], tags: &mut TrackTags) {
    let id_len = if version == 2 { 3 } else { 4 };
    let header_len = if version == 2 { 6 } else { 10 };
    let mut offset = 0;
    while offset + header_len <= body.len() {
        let id = &body[offset..offset + id_len];
        // Tags are padded with zeros to leave room for later edits, so a zero id is the end.
        if id[0] == 0 {
            return;
        }
        let size = frame_size(version, &body[offset + id_len..offset + id_len * 2]);
        let start = offset + header_len;
        let Some(end) = start.checked_add(size).filter(|end| *end <= body.len()) else {
            return;
        };
        if let Some(field) = field_of(id) {
            // The second of the two flag bytes is the one that describes the frame's data.
            let flags = if version == 2 { 0 } else { body[start - 1] };
            if let Some(frame) = frame_body(version, flags, &body[start..end]) {
                assign(field, frame, tags);
            }
        }
        offset = end;
    }
}

fn frame_size(version: u8, bytes: &[u8]) -> usize {
    match version {
        2 => usize::from(bytes[0]) << 16 | usize::from(bytes[1]) << 8 | usize::from(bytes[2]),
        3 => u32::from_be_bytes(bytes.try_into().expect("4 bytes")) as usize,
        _ => syncsafe(bytes) as usize,
    }
}

/// 2.4 lets a frame be compressed, encrypted, or prefixed with its decoded length. The first
/// two are not worth carrying a decompressor for; the third is four bytes to step over.
fn frame_body(version: u8, flags: u8, body: &[u8]) -> Option<&[u8]> {
    if version < 4 {
        return Some(body);
    }
    if flags & 0x0C != 0 {
        return None;
    }
    if flags & 0x01 == 0 {
        Some(body)
    } else {
        body.get(4..)
    }
}

fn assign(field: Field, frame: &[u8], tags: &mut TrackTags) {
    let Some(text) = decode_text(frame) else {
        return;
    };
    match field {
        Field::Title => tags.title = Some(text),
        Field::Artist => tags.artist = Some(text),
        Field::Album => tags.album = Some(text),
        // A track frame reads "5" or "5/12", and only the first half is a number.
        Field::Track => tags.track = text.split('/').next().and_then(|n| n.parse().ok()),
    }
}

fn decode_text(frame: &[u8]) -> Option<String> {
    let (encoding, bytes) = frame.split_first()?;
    let text = match encoding {
        0 => bytes.iter().map(|byte| char::from(*byte)).collect(),
        1 => utf16_with_bom(bytes),
        2 => utf16(bytes, u16::from_be_bytes),
        3 => String::from_utf8_lossy(bytes).into_owned(),
        _ => return None,
    };
    // 2.4 separates multiple values with nulls, and 2.3 pads the end with one.
    let value = text.split('\0').find(|part| !part.trim().is_empty())?;
    Some(value.trim().to_owned())
}

fn utf16_with_bom(bytes: &[u8]) -> String {
    match bytes.get(0..2) {
        Some([0xFF, 0xFE]) => utf16(&bytes[2..], u16::from_le_bytes),
        Some([0xFE, 0xFF]) => utf16(&bytes[2..], u16::from_be_bytes),
        _ => utf16(bytes, u16::from_le_bytes),
    }
}

fn utf16(bytes: &[u8], order: fn([u8; 2]) -> u16) -> String {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks_exact(2) {
        units.push(order([pair[0], pair[1]]));
    }
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use crate::reader::tags::{TrackTags, parse_diin, read_id3v2};

    /// Build an ID3v2.3 frame holding ISO-8859-1 text.
    fn frame(id: &[u8; 4], text: &str) -> Vec<u8> {
        let mut body = vec![0_u8];
        body.extend_from_slice(text.as_bytes());
        let mut out = Vec::new();
        out.extend_from_slice(id);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&body);
        out
    }

    fn tag(version: u8, flags: u8, frames: Vec<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"ID3");
        out.extend_from_slice(&[version, 0, flags]);
        let size = frames.len() as u32;
        for shift in [21, 14, 7, 0] {
            out.push(((size >> shift) & 0x7F) as u8);
        }
        out.extend_from_slice(&frames);
        out
    }

    fn read(bytes: Vec<u8>) -> TrackTags {
        read_id3v2(&mut Cursor::new(bytes), 0).expect("reads")
    }

    #[test]
    fn text_frames_become_the_four_fields_shown() {
        let mut frames = frame(b"TIT2", "Blue in Green");
        frames.extend(frame(b"TPE1", "Miles Davis"));
        frames.extend(frame(b"TALB", "Kind of Blue"));
        frames.extend(frame(b"TRCK", "3/5"));

        let tags = read(tag(3, 0, frames));

        assert_eq!(tags.title.as_deref(), Some("Blue in Green"));
        assert_eq!(tags.artist.as_deref(), Some("Miles Davis"));
        assert_eq!(tags.album.as_deref(), Some("Kind of Blue"));
        assert_eq!(tags.track, Some(3));
        assert_eq!(
            tags.headline().as_deref(),
            Some("Miles Davis — Blue in Green")
        );
        assert_eq!(tags.label().as_deref(), Some(" 3. Blue in Green"));
    }

    #[test]
    fn a_file_with_no_tag_reads_as_no_tags() {
        let tags = read(b"not a tag at all".to_vec());

        assert_eq!(tags, TrackTags::default());
        assert_eq!(tags.headline(), None);
    }

    #[test]
    fn utf16_text_with_a_byte_order_mark_is_decoded() {
        let mut body = vec![1_u8, 0xFF, 0xFE];
        for unit in "Água".encode_utf16() {
            body.extend_from_slice(&unit.to_le_bytes());
        }
        let mut frames = Vec::new();
        frames.extend_from_slice(b"TIT2");
        frames.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frames.extend_from_slice(&[0, 0]);
        frames.extend_from_slice(&body);

        let tags = read(tag(3, 0, frames));

        assert_eq!(tags.title.as_deref(), Some("Água"));
    }

    #[test]
    fn utf8_text_is_decoded_and_trailing_nulls_are_dropped() {
        let mut frames = Vec::new();
        let body = b"\x03Kind of Blue\0";
        frames.extend_from_slice(b"TALB");
        frames.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frames.extend_from_slice(&[0, 0]);
        frames.extend_from_slice(body);

        let tags = read(tag(4, 0, frames));

        assert_eq!(tags.album.as_deref(), Some("Kind of Blue"));
    }

    #[test]
    fn a_version_2_2_tag_uses_its_three_character_frame_ids() {
        let mut frames = Vec::new();
        let body = b"\x00So What";
        frames.extend_from_slice(b"TT2");
        frames.extend_from_slice(&[0, 0, body.len() as u8]);
        frames.extend_from_slice(body);

        let tags = read(tag(2, 0, frames));

        assert_eq!(tags.title.as_deref(), Some("So What"));
    }

    #[test]
    fn an_unsynchronised_tag_is_restored_before_the_frames_are_read() {
        let mut frames = Vec::new();
        frames.extend_from_slice(b"TIT2");
        frames.extend_from_slice(&2_u32.to_be_bytes());
        frames.extend_from_slice(&[0, 0]);
        // A 0xFF byte of text, escaped the way an unsynchronised tag stores it.
        frames.extend_from_slice(&[0x00, 0xFF, 0x00]);

        let tags = read(tag(3, 0x80, frames));

        assert_eq!(tags.title.as_deref(), Some("\u{FF}"));
    }

    #[test]
    fn padding_after_the_last_frame_ends_the_walk() {
        let mut frames = frame(b"TIT2", "Flamenco Sketches");
        frames.extend_from_slice(&[0; 64]);

        let tags = read(tag(3, 0, frames));

        assert_eq!(tags.title.as_deref(), Some("Flamenco Sketches"));
        assert_eq!(tags.artist, None);
    }

    #[test]
    fn a_frame_running_past_the_tag_is_ignored_rather_than_panicking() {
        let mut frames = Vec::new();
        frames.extend_from_slice(b"TIT2");
        frames.extend_from_slice(&9_999_u32.to_be_bytes());
        frames.extend_from_slice(&[0, 0, 0]);

        let tags = read(tag(3, 0, frames));

        assert_eq!(tags, TrackTags::default());
    }

    #[test]
    fn diin_sub_chunks_give_the_artist_and_title() {
        let mut body = Vec::new();
        for (id, text) in [(b"DIAR", "Bill Evans"), (b"DITI", "Peace Piece")] {
            body.extend_from_slice(id);
            body.extend_from_slice(&((text.len() + 4) as u64).to_be_bytes());
            body.extend_from_slice(&(text.len() as u32).to_be_bytes());
            body.extend_from_slice(text.as_bytes());
        }
        let mut tags = TrackTags::default();

        parse_diin(&body, &mut tags);

        assert_eq!(tags.artist.as_deref(), Some("Bill Evans"));
        assert_eq!(tags.title.as_deref(), Some("Peace Piece"));
    }
}
