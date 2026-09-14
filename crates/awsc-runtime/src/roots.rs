//! The trust store every TLS connection is verified against.
//!
//! There are three sources and a strict order:
//!
//! 1. `--ca-bundle` / `AWS_CA_BUNDLE`, when given. Nothing else is consulted — the point
//!    of naming a bundle is to be the only thing trusted.
//! 2. The system certificate store.
//! 3. **The compiled-in Mozilla root program**, when the system has no store at all.
//!
//! The third exists because without it a host with no `ca-certificates` package cannot
//! make a single HTTPS call — every request fails with "no native root CA certificates
//! found", including requests to a plain `http://` endpoint, since the TLS configuration
//! is built before the scheme is looked at. The reference does not have this problem:
//! botocore trusts `certifi`, a bundle shipped inside the package, and never looks at the
//! system store at all.
//!
//! Note what that means for the *order*: this prefers the system store where there is
//! one, which the reference does not. A corporate root installed system-wide is therefore
//! trusted here and would need `AWS_CA_BUNDLE` with the real `aws`. That is a difference
//! in our favour, and it is recorded in `docs/divergences.md`.

/// Where the roots in a store came from, for diagnostics and tests.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum RootSource {
    Bundle,
    System,
    /// The system store was absent or empty, so the compiled-in roots were used.
    Compiled,
}

/// Build the trust store, saying which source supplied it.
pub fn load(ca_bundle: Option<&str>) -> Result<(rustls::RootCertStore, RootSource), String> {
    let mut roots = rustls::RootCertStore::empty();

    if let Some(path) = ca_bundle {
        let file = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
        let mut reader = std::io::BufReader::new(file);
        let mut added = 0usize;
        for cert in rustls_pemfile::certs(&mut reader) {
            let cert = cert.map_err(|e| format!("{path}: {e}"))?;
            roots.add(cert).map_err(|e| format!("{path}: {e}"))?;
            added += 1;
        }
        // An empty or non-PEM file would otherwise produce a store that trusts nothing,
        // and every request would fail with an opaque certificate error rather than
        // naming the real problem.
        if added == 0 {
            return Err(format!("{path}: no PEM certificates found"));
        }
        return Ok((roots, RootSource::Bundle));
    }

    let loaded = rustls_native_certs::load_native_certs();
    for cert in loaded.certs {
        // A malformed certificate in the system store is skipped rather than fatal: one
        // bad entry should not take down a store that has hundreds of good ones.
        let _ = roots.add(cert);
    }
    if !roots.is_empty() {
        return Ok((roots, RootSource::System));
    }

    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if roots.is_empty() {
        return Err("no trusted certificate authorities are available".to_string());
    }
    Ok((roots, RootSource::Compiled))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The point of the whole module: a store is always available, whatever the host has.
    #[test]
    fn there_is_always_a_trust_store() {
        let (roots, source) = load(None).expect("a store");
        assert!(!roots.is_empty());
        assert!(matches!(source, RootSource::System | RootSource::Compiled));
    }

    /// The compiled-in roots are real and numerous — a fallback that trusted nothing
    /// would turn a clear error into an opaque handshake failure.
    #[test]
    fn the_compiled_roots_are_a_usable_store() {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        assert!(roots.len() > 50, "only {} compiled roots", roots.len());
    }

    /// A named bundle is the only thing trusted, and an unusable one is named rather than
    /// silently leaving an empty store behind.
    #[test]
    fn a_named_bundle_replaces_everything_and_must_be_usable() {
        let directory = std::env::temp_dir().join(format!("awsc-roots-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("creates");

        let empty = directory.join("empty.pem");
        std::fs::write(&empty, b"").expect("writes");
        let failure = load(Some(&empty.to_string_lossy())).expect_err("refuses");
        assert!(failure.contains("no PEM certificates found"), "{failure}");

        let missing = directory.join("absent.pem");
        assert!(load(Some(&missing.to_string_lossy())).is_err());

        let _ = std::fs::remove_dir_all(&directory);
    }
}
