//! A minimal ZIP writer, for the commands that bundle a directory before uploading it.
//!
//! `deploy push` and `gamelift upload-build` both zip a source tree. Python's `zipfile`
//! does it for the reference; this is the same container written by hand, because the
//! alternative is a dependency that would be used for exactly two commands.
//!
//! What it writes is a deflate-compressed ZIP with the sizes in each local header (no
//! data descriptors), which is what `zipfile.ZipFile(..., 'w')` produces and what every
//! unzip implementation reads.
//!
//! **The 4 GiB ceiling is real**: Python passes `allowZip64=True` and would keep going,
//! where this refuses. A deployment bundle that large is not something to discover at the
//! far end of an upload, so the refusal is explicit and early — see `docs/divergences.md`.

use std::io::Write;

/// One entry, already compressed.
struct Entry {
    name: String,
    crc32: u32,
    compressed: Vec<u8>,
    uncompressed_size: u32,
    offset: u32,
    dos_time: u16,
    dos_date: u16,
}

#[derive(Default)]
pub struct Archive {
    entries: Vec<Entry>,
    body: Vec<u8>,
}

/// Why a bundle could not be written.
#[derive(Debug)]
pub enum ZipError {
    /// The archive, or a file in it, is at or past the 4 GiB the format can address.
    TooLarge(String),
    Io(std::io::Error),
}

impl std::fmt::Display for ZipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZipError::TooLarge(what) => write!(
                f,
                "{what} is too large for a zip archive (4 GiB is the limit without ZIP64)"
            ),
            ZipError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl Archive {
    pub fn new() -> Archive {
        Archive::default()
    }

    /// Add one file under `name`, which is the path *inside* the archive.
    ///
    /// `modified` is seconds since the epoch; anything before 1980 is clamped, because
    /// the DOS timestamp a ZIP carries cannot represent it.
    pub fn add(&mut self, name: &str, contents: &[u8], modified: i64) -> Result<(), ZipError> {
        if contents.len() > u32::MAX as usize {
            return Err(ZipError::TooLarge(name.to_string()));
        }
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(contents).map_err(ZipError::Io)?;
        let compressed = encoder.finish().map_err(ZipError::Io)?;

        let mut crc = flate2::Crc::new();
        crc.update(contents);
        let (dos_time, dos_date) = dos_timestamp(modified);

        let offset = u32::try_from(self.body.len())
            .map_err(|_| ZipError::TooLarge("the archive".to_string()))?;
        // The local header carries the sizes, so no data descriptor is needed.
        self.body.extend_from_slice(&local_header(
            name,
            crc.sum(),
            compressed.len() as u32,
            contents.len() as u32,
            dos_time,
            dos_date,
        ));
        self.body.extend_from_slice(&compressed);

        self.entries.push(Entry {
            name: name.to_string(),
            crc32: crc.sum(),
            uncompressed_size: contents.len() as u32,
            compressed,
            offset,
            dos_time,
            dos_date,
        });
        Ok(())
    }

    /// The finished archive.
    pub fn finish(self) -> Result<Vec<u8>, ZipError> {
        let mut out = self.body;
        let directory_offset = u32::try_from(out.len())
            .map_err(|_| ZipError::TooLarge("the archive".to_string()))?;
        for entry in &self.entries {
            out.extend_from_slice(&central_header(entry));
        }
        let directory_size = u32::try_from(out.len() - directory_offset as usize)
            .map_err(|_| ZipError::TooLarge("the archive".to_string()))?;
        let count = u16::try_from(self.entries.len())
            .map_err(|_| ZipError::TooLarge("the archive's file count".to_string()))?;

        // End of central directory.
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        out.extend_from_slice(&0u16.to_le_bytes()); // disk with the directory
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&directory_size.to_le_bytes());
        out.extend_from_slice(&directory_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        Ok(out)
    }
}

fn local_header(
    name: &str,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    dos_time: u16,
    dos_date: u16,
) -> Vec<u8> {
    let mut header = Vec::with_capacity(30 + name.len());
    header.extend_from_slice(b"PK\x03\x04");
    header.extend_from_slice(&20u16.to_le_bytes()); // version needed: 2.0, deflate
    header.extend_from_slice(&0u16.to_le_bytes()); // flags
    header.extend_from_slice(&8u16.to_le_bytes()); // method: deflate
    header.extend_from_slice(&dos_time.to_le_bytes());
    header.extend_from_slice(&dos_date.to_le_bytes());
    header.extend_from_slice(&crc32.to_le_bytes());
    header.extend_from_slice(&compressed_size.to_le_bytes());
    header.extend_from_slice(&uncompressed_size.to_le_bytes());
    header.extend_from_slice(&(name.len() as u16).to_le_bytes());
    header.extend_from_slice(&0u16.to_le_bytes()); // extra field length
    header.extend_from_slice(name.as_bytes());
    header
}

fn central_header(entry: &Entry) -> Vec<u8> {
    let mut header = Vec::with_capacity(46 + entry.name.len());
    header.extend_from_slice(b"PK\x01\x02");
    header.extend_from_slice(&20u16.to_le_bytes()); // version made by
    header.extend_from_slice(&20u16.to_le_bytes()); // version needed
    header.extend_from_slice(&0u16.to_le_bytes()); // flags
    header.extend_from_slice(&8u16.to_le_bytes()); // deflate
    header.extend_from_slice(&entry.dos_time.to_le_bytes());
    header.extend_from_slice(&entry.dos_date.to_le_bytes());
    header.extend_from_slice(&entry.crc32.to_le_bytes());
    header.extend_from_slice(&(entry.compressed.len() as u32).to_le_bytes());
    header.extend_from_slice(&entry.uncompressed_size.to_le_bytes());
    header.extend_from_slice(&(entry.name.len() as u16).to_le_bytes());
    header.extend_from_slice(&0u16.to_le_bytes()); // extra
    header.extend_from_slice(&0u16.to_le_bytes()); // comment
    header.extend_from_slice(&0u16.to_le_bytes()); // disk number
    header.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
    header.extend_from_slice(&0u32.to_le_bytes()); // external attributes
    header.extend_from_slice(&entry.offset.to_le_bytes());
    header.extend_from_slice(entry.name.as_bytes());
    header
}

/// The MS-DOS time and date a ZIP entry carries: two-second resolution, and years
/// counted from 1980, which is why nothing older can be represented.
fn dos_timestamp(unix: i64) -> (u16, u16) {
    let (year, month, day, hour, minute, second) = civil(unix);
    if year < 1980 {
        // 1980-01-01 00:00:00, the earliest the format has.
        return (0, 0x21);
    }
    let time = ((hour as u16) << 11) | ((minute as u16) << 5) | ((second / 2) as u16);
    let date = (((year - 1980) as u16) << 9) | ((month as u16) << 5) | (day as u16);
    (time, date)
}

/// Split a unix timestamp into civil components, in UTC.
fn civil(unix: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = unix.div_euclid(86_400);
    let seconds = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, seconds / 3600, (seconds % 3600) / 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut archive = Archive::new();
        for (name, contents) in files {
            archive.add(name, contents, 1_600_000_000).expect("adds");
        }
        archive.finish().expect("finishes")
    }

    #[test]
    fn it_writes_the_signatures_a_zip_reader_looks_for() {
        let bytes = archive_of(&[("appspec.yml", b"version: 0.0\n")]);
        assert_eq!(&bytes[0..4], b"PK\x03\x04");
        // The central directory and the end-of-directory record are both present.
        assert!(bytes.windows(4).any(|w| w == b"PK\x01\x02"));
        assert!(bytes.windows(4).any(|w| w == b"PK\x05\x06"));
    }

    /// The end record has to agree with the directory it points at, or a reader sees a
    /// truncated archive.
    #[test]
    fn the_end_record_counts_and_locates_the_directory() {
        let bytes = archive_of(&[("a.txt", b"aaa"), ("b/c.txt", b"bbb")]);
        let eocd = bytes.len() - 22;
        assert_eq!(&bytes[eocd..eocd + 4], b"PK\x05\x06");
        let count = u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]);
        assert_eq!(count, 2);
        let size =
            u32::from_le_bytes(bytes[eocd + 12..eocd + 16].try_into().expect("4 bytes")) as usize;
        let offset =
            u32::from_le_bytes(bytes[eocd + 16..eocd + 20].try_into().expect("4 bytes")) as usize;
        assert_eq!(offset + size, eocd);
        assert_eq!(&bytes[offset..offset + 4], b"PK\x01\x02");
    }

    /// Two-second resolution and years from 1980 — anything older cannot be written, so
    /// it is clamped rather than wrapping into a nonsense date.
    #[test]
    fn dos_timestamps_clamp_at_1980() {
        // 2020-09-13T12:26:40Z
        let (time, date) = dos_timestamp(1_600_000_000);
        assert_eq!(date >> 9, 2020 - 1980);
        assert_eq!((date >> 5) & 0xF, 9);
        assert_eq!(date & 0x1F, 13);
        assert_eq!(time >> 11, 12);
        assert_eq!((time >> 5) & 0x3F, 26);
        // 1970 is before the format's epoch.
        assert_eq!(dos_timestamp(0), (0, 0x21));
    }

    #[test]
    fn an_empty_archive_is_just_the_end_record() {
        let bytes = archive_of(&[]);
        assert_eq!(bytes.len(), 22);
        assert_eq!(&bytes[0..4], b"PK\x05\x06");
    }
}
