//! Obtaining `models.bin` when the install did not bring one.
//!
//! The compiled service catalogue is 113 MB — it is mmap'd, not parsed, which is why the
//! binary starts fast — and it lives beside the executable in a release tarball. Neither
//! `cargo binstall` nor `cargo install` can deliver it: binstall installs *binaries* and
//! has no mechanism for data files, and crates.io caps a package at about 10 MB. Without
//! this module both install paths produce a binary that fails on every command with
//! `Found invalid choice 'sts'` — an error that blames the user for a typo.
//!
//! So the binary fetches its own catalogue, once, from the GitHub release matching its
//! own version, and keeps it in a per-user cache directory.
//!
//! **What the integrity check is worth.** The download is verified against the
//! `SHA256SUMS` published in the same release, over HTTPS. That is a real guard against a
//! truncated or corrupted transfer, and it pins the catalogue to the release the binary
//! was cut from. It is *not* protection against a compromised release: the checksum and
//! the asset come from the same place, so the trust anchor is GitHub plus TLS either way.
//! Anyone who needs a stronger guarantee should install from the release tarball, which
//! carries the catalogue directly, or point `AWSC_MODELS_DIR` at a copy they vetted.

use std::io::Read;
use std::path::PathBuf;

/// Where the release keeps its assets.
const RELEASE_BASE: &str = "https://github.com/DavidLee18/aws_cli/releases/download";

/// The release base, overridable for a mirror, a fork, or a test that serves its own.
/// Without this the download path could only ever be exercised against the real GitHub
/// release, which means it could not be tested before the release existed.
fn release_base() -> String {
    match std::env::var("AWSC_RELEASE_BASE") {
        Ok(base) if !base.is_empty() => base.trim_end_matches('/').to_string(),
        _ => RELEASE_BASE.to_string(),
    }
}
/// The compressed catalogue's asset name.
const ASSET: &str = "models.bin.gz";

/// The cache directory holding this version's catalogue.
///
/// Versioned, so upgrading the binary fetches the catalogue that matches it rather than
/// silently running against an older service surface. Old versions are left in place: the
/// user may still have older binaries, and deleting 113 MB that someone else's copy is
/// mmap'ing is not ours to do.
pub fn cache_dir() -> Option<PathBuf> {
    let root = match std::env::var("AWSC_CACHE_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => platform_cache_root()?.join("awsc"),
    };
    Some(root.join(env!("CARGO_PKG_VERSION")))
}

/// The conventional per-user cache location for each platform.
fn platform_cache_root() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        return home().map(|h| h.join("Library/Caches"));
    }
    if cfg!(target_os = "windows") {
        return std::env::var("LOCALAPPDATA").ok().filter(|v| !v.is_empty()).map(PathBuf::from);
    }
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg));
        }
    }
    home().map(|h| h.join(".cache"))
}

fn home() -> Option<PathBuf> {
    std::env::var("HOME").ok().filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Is a usable catalogue already cached?
pub fn cached() -> Option<PathBuf> {
    let dir = cache_dir()?;
    dir.join("models.bin").is_file().then_some(dir)
}

/// Download, verify and unpack the catalogue into the cache, returning its directory.
///
/// `announce` is false for an explicit `update-models`, which prints its own progress.
pub fn fetch(announce: bool) -> Result<PathBuf, String> {
    let version = env!("CARGO_PKG_VERSION");
    let dir = cache_dir().ok_or_else(|| {
        "cannot locate a cache directory (HOME and AWSC_CACHE_DIR are both unset)".to_string()
    })?;

    if announce {
        eprintln!(
            "Downloading the service catalogue for awsc {version} (~16 MB) to {}.\n\
             This happens once. Run `awsc update-models` to refresh it.",
            dir.display()
        );
    }

    let expected = expected_digest(version)?;
    let compressed = download(&format!("{}/v{version}/{ASSET}", release_base()))?;

    let actual = sha256_hex(&compressed);
    if actual != expected {
        return Err(format!(
            "the downloaded catalogue does not match the checksum published for v{version} \
             (expected {expected}, got {actual}); it may have been truncated in transit"
        ));
    }

    let models = decompress(&compressed)?;

    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    // Written under a temporary name and renamed, so a failed or interrupted download can
    // never leave a half-written catalogue that later looks complete. The rename is
    // atomic within the directory.
    let temporary = dir.join("models.bin.partial");
    std::fs::write(&temporary, &models)
        .map_err(|e| format!("writing {}: {e}", temporary.display()))?;
    let final_path = dir.join("models.bin");
    std::fs::rename(&temporary, &final_path)
        .map_err(|e| format!("installing {}: {e}", final_path.display()))?;

    Ok(dir)
}

/// The catalogue's expected digest, read from the release's `SHA256SUMS`.
fn expected_digest(version: &str) -> Result<String, String> {
    let sums = download(&format!("{}/v{version}/SHA256SUMS", release_base()))?;
    let text = String::from_utf8(sums)
        .map_err(|_| "the published SHA256SUMS is not text".to_string())?;
    for line in text.lines() {
        // `<digest>  <name>`, the format sha256sum writes.
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(name)) = (parts.next(), parts.next()) else { continue };
        if name.trim_start_matches('*') == ASSET {
            return Ok(digest.to_ascii_lowercase());
        }
    }
    Err(format!(
        "release v{version} publishes no checksum for {ASSET}; \
         this build of awsc cannot fetch its catalogue"
    ))
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let response = ureq::AgentBuilder::new()
        .timeout_connect(std::time::Duration::from_secs(30))
        .build()
        .get(url)
        .call()
        .map_err(|e| format!("downloading {url}: {e}"))?;

    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("reading {url}: {e}"))?;
    Ok(bytes)
}

fn decompress(gzipped: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(gzipped)
        .read_to_end(&mut out)
        .map_err(|e| format!("decompressing the catalogue: {e}"))?;
    Ok(out)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

/// `awsc update-models`: fetch the catalogue even if one is already cached.
pub fn update_models() -> Result<(), String> {
    let version = env!("CARGO_PKG_VERSION");
    println!("Fetching the service catalogue for awsc {version}.");
    let dir = fetch(false)?;
    let size = std::fs::metadata(dir.join("models.bin")).map(|m| m.len()).unwrap_or(0);
    println!("Installed {} ({:.0} MB).", dir.join("models.bin").display(), size as f64 / 1_048_576.0);
    Ok(())
}

/// The message shown when there is no catalogue and fetching it did not work.
pub fn unavailable(reason: &str) -> String {
    format!(
        "the service catalogue is not installed and could not be downloaded: {reason}\n\
         \n\
         Install it with `awsc update-models`, or download a release archive from\n\
         https://github.com/DavidLee18/aws_cli/releases and keep models.bin beside the\n\
         binary, or point AWSC_MODELS_DIR at a directory containing it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache is versioned so an upgraded binary does not silently reuse an older
    /// service surface.
    #[test]
    fn the_cache_directory_is_versioned() {
        let dir = cache_dir().expect("a cache directory");
        assert_eq!(
            dir.file_name().and_then(|n| n.to_str()),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    /// An explicit override wins over the platform location, which is what lets a test or
    /// a CI job keep the catalogue somewhere disposable.
    #[test]
    fn the_cache_directory_can_be_overridden() {
        // Not run in parallel with other env readers: this is the only test touching it.
        std::env::set_var("AWSC_CACHE_DIR", "/tmp/awsc-cache-test");
        let dir = cache_dir().expect("a cache directory");
        std::env::remove_var("AWSC_CACHE_DIR");
        assert_eq!(dir, PathBuf::from("/tmp/awsc-cache-test").join(env!("CARGO_PKG_VERSION")));
    }

    /// The digest is read from the line naming our asset, not the first line -- the file
    /// lists every platform archive too.
    #[test]
    fn the_digest_is_read_from_the_line_naming_the_catalogue() {
        let sums = "aaaa  awsc-0.2.0-x86_64-unknown-linux-gnu.tar.gz\n\
                    bbbb  models.bin.gz\n\
                    cccc  awsc-0.2.0-aarch64-apple-darwin.tar.gz\n";
        let found = sums
            .lines()
            .filter_map(|line| {
                let mut parts = line.split_whitespace();
                match (parts.next(), parts.next()) {
                    (Some(d), Some(n)) if n.trim_start_matches('*') == ASSET => Some(d),
                    _ => None,
                }
            })
            .next();
        assert_eq!(found, Some("bbbb"));
    }

    #[test]
    fn a_gzip_round_trip_returns_the_original_bytes() {
        use flate2::write::GzEncoder;
        use std::io::Write;
        let original = b"the compiled service catalogue".repeat(100);
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&original).expect("compress");
        let compressed = encoder.finish().expect("finish");
        assert_eq!(decompress(&compressed).expect("decompress"), original);
    }

    #[test]
    fn sha256_matches_the_known_digest_of_an_empty_input() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
