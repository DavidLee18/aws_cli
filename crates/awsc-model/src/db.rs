//! The compiled model container: all 431 service models in one memory-mapped file.
//!
//! Loading a service used to mean `serde_json` parsing a 3.7–7.6 MB document on every
//! invocation, which was ~21 ms of the 26 ms it took `s3api` to build a request — and it
//! parsed all ~15,000 shapes to reach the two hundred a command actually touches.
//!
//! This file is mapped, not read. Finding a service is a binary search over a sorted
//! directory; finding a shape within it is another. Only the shapes a command reaches get
//! decoded, and only the pages holding them are ever faulted in.
//!
//! Reader and writer live together on purpose. A container format described in two places
//! drifts, and a drifted offset table does not fail loudly — it silently returns the wrong
//! shape.

use std::path::Path;

pub const MAGIC: &[u8; 8] = b"AWSCMDL1";

/// `name_off, name_len, blob_off, blob_len`
const SERVICE_ENTRY: usize = 16;
/// `id_off, id_len, json_off, json_len`
const SHAPE_ENTRY: usize = 16;
/// `name_off, name_len, id_off, id_len`
const OP_ENTRY: usize = 16;

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

fn str_at(bytes: &[u8], off: u32, len: u32) -> Option<&str> {
    std::str::from_utf8(bytes.get(off as usize..(off + len) as usize)?).ok()
}

// ---------------------------------------------------------------- writing

/// Builds a container. Strings are interned, which matters because shape ids repeat
/// heavily across a service's directory and operation index.
#[derive(Default)]
pub struct Writer {
    services: Vec<PendingService>,
}

struct PendingService {
    cli_name: String,
    service_id: String,
    /// `(shape id, the shape's JSON object)`, unsorted on the way in.
    shapes: Vec<(String, String)>,
    /// `(CLI operation name, operation shape id)`.
    operations: Vec<(String, String)>,
}

/// Appends bytes to an arena, returning `(offset, len)` and reusing an identical run when
/// one is already present.
struct Arena {
    bytes: Vec<u8>,
    interned: std::collections::HashMap<String, u32>,
}

impl Arena {
    fn new() -> Arena {
        Arena { bytes: Vec::new(), interned: std::collections::HashMap::new() }
    }

    fn push(&mut self, s: &str) -> (u32, u32) {
        if let Some(&off) = self.interned.get(s) {
            return (off, s.len() as u32);
        }
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        self.interned.insert(s.to_string(), off);
        (off, s.len() as u32)
    }
}

impl Writer {
    pub fn new() -> Writer {
        Writer::default()
    }

    pub fn add_service(
        &mut self,
        cli_name: String,
        service_id: String,
        shapes: Vec<(String, String)>,
        operations: Vec<(String, String)>,
    ) {
        self.services.push(PendingService { cli_name, service_id, shapes, operations });
    }

    pub fn finish(mut self) -> Vec<u8> {
        // Both directories are binary-searched at read time, so both must be sorted by the
        // exact bytes the reader compares.
        self.services.sort_by(|a, b| a.cli_name.cmp(&b.cli_name));
        for service in &mut self.services {
            service.shapes.sort_by(|a, b| a.0.cmp(&b.0));
            service.operations.sort_by(|a, b| a.0.cmp(&b.0));
        }

        // Two passes: the directories hold absolute offsets, so the arena has to be laid
        // out before the header can be written. Sizes are known from the counts.
        let header_len = 8 + 4;
        let service_dir_len = self.services.len() * SERVICE_ENTRY;
        let mut blob_len = 0usize;
        for service in &self.services {
            blob_len += 16 + service.shapes.len() * SHAPE_ENTRY + service.operations.len() * OP_ENTRY;
        }
        let arena_base = (header_len + service_dir_len + blob_len) as u32;

        let mut arena = Arena::new();
        let mut service_dir = Vec::with_capacity(service_dir_len);
        let mut blobs = Vec::with_capacity(blob_len);
        let mut blob_cursor = (header_len + service_dir_len) as u32;

        for service in &self.services {
            let (name_off, name_len) = arena.push(&service.cli_name);
            let this_blob_len =
                16 + service.shapes.len() * SHAPE_ENTRY + service.operations.len() * OP_ENTRY;

            service_dir.extend_from_slice(&(name_off + arena_base).to_le_bytes());
            service_dir.extend_from_slice(&name_len.to_le_bytes());
            service_dir.extend_from_slice(&blob_cursor.to_le_bytes());
            service_dir.extend_from_slice(&(this_blob_len as u32).to_le_bytes());
            blob_cursor += this_blob_len as u32;

            let (id_off, id_len) = arena.push(&service.service_id);
            blobs.extend_from_slice(&(id_off + arena_base).to_le_bytes());
            blobs.extend_from_slice(&id_len.to_le_bytes());
            blobs.extend_from_slice(&(service.shapes.len() as u32).to_le_bytes());
            blobs.extend_from_slice(&(service.operations.len() as u32).to_le_bytes());

            for (id, json) in &service.shapes {
                let (id_off, id_len) = arena.push(id);
                let (json_off, json_len) = arena.push(json);
                blobs.extend_from_slice(&(id_off + arena_base).to_le_bytes());
                blobs.extend_from_slice(&id_len.to_le_bytes());
                blobs.extend_from_slice(&(json_off + arena_base).to_le_bytes());
                blobs.extend_from_slice(&json_len.to_le_bytes());
            }
            for (name, id) in &service.operations {
                let (name_off, name_len) = arena.push(name);
                let (id_off, id_len) = arena.push(id);
                blobs.extend_from_slice(&(name_off + arena_base).to_le_bytes());
                blobs.extend_from_slice(&name_len.to_le_bytes());
                blobs.extend_from_slice(&(id_off + arena_base).to_le_bytes());
                blobs.extend_from_slice(&id_len.to_le_bytes());
            }
        }

        let mut out = Vec::with_capacity(arena_base as usize + arena.bytes.len());
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.services.len() as u32).to_le_bytes());
        out.extend_from_slice(&service_dir);
        out.extend_from_slice(&blobs);
        debug_assert_eq!(out.len(), arena_base as usize);
        out.extend_from_slice(&arena.bytes);
        out
    }
}

// ---------------------------------------------------------------- reading

pub struct ModelDb {
    map: memmap2::Mmap,
}

impl ModelDb {
    pub fn open(path: &Path) -> Result<ModelDb, String> {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("cannot open {} ({e})", path.display()))?;
        // Safety: the container is a build artifact this process does not write. A
        // concurrent truncation would be a torn read, which is why the compiler writes to
        // a temporary path and renames.
        let map = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| format!("cannot map {} ({e})", path.display()))?;
        if map.get(..8) != Some(MAGIC.as_slice()) {
            return Err(format!("{} is not a compiled model container", path.display()));
        }
        Ok(ModelDb { map })
    }

    fn service_count(&self) -> usize {
        u32_at(&self.map, 8).unwrap_or(0) as usize
    }

    /// Every CLI service name in the container, in sorted order.
    pub fn service_names(&self) -> impl Iterator<Item = &str> {
        (0..self.service_count()).filter_map(|i| self.service_name_at(i))
    }

    fn service_name_at(&self, index: usize) -> Option<&str> {
        let at = 12 + index * SERVICE_ENTRY;
        str_at(&self.map, u32_at(&self.map, at)?, u32_at(&self.map, at + 4)?)
    }

    pub fn service(&self, cli_name: &str) -> Option<ServiceView<'_>> {
        let count = self.service_count();
        let mut lo = 0usize;
        let mut hi = count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.service_name_at(mid)?.cmp(cli_name) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let at = 12 + mid * SERVICE_ENTRY;
                    let blob_off = u32_at(&self.map, at + 8)? as usize;
                    return Some(ServiceView { file: &self.map, blob: blob_off });
                }
            }
        }
        None
    }
}

/// One service inside the container. Holds no decoded state — every accessor is a lookup
/// into the mapped bytes.
#[derive(Clone, Copy)]
pub struct ServiceView<'a> {
    file: &'a [u8],
    blob: usize,
}

impl<'a> ServiceView<'a> {
    pub fn service_id(&self) -> Option<&'a str> {
        str_at(self.file, u32_at(self.file, self.blob)?, u32_at(self.file, self.blob + 4)?)
    }

    fn shape_count(&self) -> usize {
        u32_at(self.file, self.blob + 8).unwrap_or(0) as usize
    }

    fn op_count(&self) -> usize {
        u32_at(self.file, self.blob + 12).unwrap_or(0) as usize
    }

    fn shape_dir(&self) -> usize {
        self.blob + 16
    }

    fn op_dir(&self) -> usize {
        self.shape_dir() + self.shape_count() * SHAPE_ENTRY
    }

    fn shape_id_at(&self, index: usize) -> Option<&'a str> {
        let at = self.shape_dir() + index * SHAPE_ENTRY;
        str_at(self.file, u32_at(self.file, at)?, u32_at(self.file, at + 4)?)
    }

    /// The raw JSON of one shape, ready to hand to `serde_json`.
    pub fn shape_json(&self, id: &str) -> Option<&'a str> {
        let mut lo = 0usize;
        let mut hi = self.shape_count();
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.shape_id_at(mid)?.cmp(id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let at = self.shape_dir() + mid * SHAPE_ENTRY;
                    return str_at(self.file, u32_at(self.file, at + 8)?, u32_at(self.file, at + 12)?);
                }
            }
        }
        None
    }

    fn op_at(&self, index: usize) -> Option<(&'a str, &'a str)> {
        let at = self.op_dir() + index * OP_ENTRY;
        let name = str_at(self.file, u32_at(self.file, at)?, u32_at(self.file, at + 4)?)?;
        let id = str_at(self.file, u32_at(self.file, at + 8)?, u32_at(self.file, at + 12)?)?;
        Some((name, id))
    }

    /// CLI operation names, sorted. Precomputed at build time, so listing a service's
    /// operations decodes no shapes at all.
    pub fn operation_names(&self) -> impl Iterator<Item = &'a str> + '_ {
        (0..self.op_count()).filter_map(move |i| self.op_at(i).map(|(name, _)| name))
    }

    pub fn operation_shape_id(&self, cli_name: &str) -> Option<&'a str> {
        let mut lo = 0usize;
        let mut hi = self.op_count();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let (name, id) = self.op_at(mid)?;
            match name.cmp(cli_name) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(id),
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut w = Writer::new();
        // Added out of order, to prove `finish` sorts what the reader binary-searches.
        w.add_service(
            "sts".into(),
            "com.amazonaws.sts#AWSSecurityTokenServiceV20110615".into(),
            vec![
                ("com.amazonaws.sts#GetCallerIdentity".into(), r#"{"type":"operation"}"#.into()),
                ("com.amazonaws.sts#AssumeRole".into(), r#"{"type":"operation","x":1}"#.into()),
            ],
            vec![
                ("get-caller-identity".into(), "com.amazonaws.sts#GetCallerIdentity".into()),
                ("assume-role".into(), "com.amazonaws.sts#AssumeRole".into()),
            ],
        );
        w.add_service("acm".into(), "com.amazonaws.acm#Acm".into(), vec![], vec![]);
        w.finish()
    }

    /// Exercises the reader against the writer's output without touching the filesystem.
    fn view<'a>(bytes: &'a [u8], name: &str) -> Option<ServiceView<'a>> {
        let count = u32_at(bytes, 8)? as usize;
        for i in 0..count {
            let at = 12 + i * SERVICE_ENTRY;
            let n = str_at(bytes, u32_at(bytes, at)?, u32_at(bytes, at + 4)?)?;
            if n == name {
                return Some(ServiceView { file: bytes, blob: u32_at(bytes, at + 8)? as usize });
            }
        }
        None
    }

    #[test]
    fn round_trips_shapes_and_operations() {
        let bytes = sample();
        assert_eq!(&bytes[..8], MAGIC);

        let sts = view(&bytes, "sts").expect("sts present");
        assert_eq!(sts.service_id(), Some("com.amazonaws.sts#AWSSecurityTokenServiceV20110615"));
        assert_eq!(
            sts.shape_json("com.amazonaws.sts#AssumeRole"),
            Some(r#"{"type":"operation","x":1}"#)
        );
        assert_eq!(
            sts.shape_json("com.amazonaws.sts#GetCallerIdentity"),
            Some(r#"{"type":"operation"}"#)
        );
        assert_eq!(sts.shape_json("com.amazonaws.sts#Missing"), None);

        assert_eq!(
            sts.operation_shape_id("get-caller-identity"),
            Some("com.amazonaws.sts#GetCallerIdentity")
        );
        assert_eq!(sts.operation_shape_id("nope"), None);
        assert_eq!(
            sts.operation_names().collect::<Vec<_>>(),
            ["assume-role", "get-caller-identity"]
        );
    }

    /// A service with no shapes must not make the following service's blob unreadable.
    #[test]
    fn empty_service_is_addressable() {
        let bytes = sample();
        let acm = view(&bytes, "acm").expect("acm present");
        assert_eq!(acm.service_id(), Some("com.amazonaws.acm#Acm"));
        assert_eq!(acm.operation_names().count(), 0);
        assert_eq!(acm.shape_json("anything"), None);
    }
}
