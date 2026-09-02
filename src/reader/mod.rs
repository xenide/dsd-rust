pub mod dff;
pub mod dsf;

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::dsd::DsdFormat;
use crate::reader::dff::DffReader;
use crate::reader::dsf::DsfReader;

/// A source of planar, MSB-first DSD bytes.
pub trait DsdSource: Send {
    fn container(&self) -> &'static str;

    fn format(&self) -> DsdFormat;

    /// DSD bytes of audio per channel, excluding container padding.
    fn total_bytes_per_channel(&self) -> u64;

    /// Bytes each plane passed to [`DsdSource::read`] must hold.
    fn chunk_bytes(&self) -> usize;

    /// Fill the head of each plane with the next DSD bytes. Returns bytes per plane, 0 at EOF.
    fn read(&mut self, planes: &mut [Box<[u8]>]) -> Result<usize>;

    /// Move the read position to `bytes_per_channel` from the start of the audio, clamped to
    /// the file. Returns where it landed, which a container addressable only in whole blocks
    /// rounds down.
    fn seek(&mut self, bytes_per_channel: u64) -> Result<u64>;

    fn duration_secs(&self) -> f64 {
        let bits = (self.total_bytes_per_channel() * 8) as f64;
        bits / f64::from(self.format().rate.hz())
    }
}

/// Open a DSD file, dispatching on the container magic rather than the extension.
pub fn open(path: &Path) -> Result<Box<dyn DsdSource>> {
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 16, file);

    let mut magic = [0_u8; 4];
    reader
        .read_exact(&mut magic)
        .with_context(|| format!("{} is empty", path.display()))?;
    reader.seek(SeekFrom::Start(0))?;

    let context = || format!("cannot parse {}", path.display());
    match &magic {
        b"DSD " => Ok(Box::new(DsfReader::new(reader).with_context(context)?)),
        b"FRM8" => Ok(Box::new(DffReader::new(reader).with_context(context)?)),
        other => bail!(
            "{}: unrecognised container (magic {:02X?}); expected DSF or DSDIFF",
            path.display(),
            other
        ),
    }
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
