//! Ed25519 keys in OpenSSH's own formats, for `ec2-instance-connect ssh`.
//!
//! That command generates a throwaway key pair, pushes the public half to the instance
//! through `send-ssh-public-key`, and hands the private half to `ssh -i`. Both halves
//! therefore have to be in the exact shapes OpenSSH reads: the `ssh-ed25519 AAAA...`
//! authorized-keys line, and the `openssh-key-v1` private-key container.
//!
//! The reference gets both from `awscrt`'s `ED25519ExportFormat.OPENSSH_B64`. There is no
//! equivalent here, so the containers are written by hand — and the test that settles it
//! is `ssh-keygen -y`, which derives the public key from our private file using OpenSSH's
//! own parser and has to produce the line we generated.

use base64ct::Encoding;

/// The two halves of an Ed25519 key. The seed *is* the private key; the public half is
/// derived from it rather than stored separately.
pub struct Ed25519Key {
    pub seed: [u8; 32],
    pub public: [u8; 32],
    /// The `check` value OpenSSH repeats twice inside the private section, so a decrypted
    /// blob can be recognised as correctly decrypted. Random, and kept so the encoding is
    /// a pure function of the key for tests.
    pub check: u32,
}

/// A fresh key, with its randomness from the TLS provider's CSPRNG.
pub fn generate() -> Result<Ed25519Key, String> {
    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let mut seed = [0u8; 32];
    provider.secure_random.fill(&mut seed).map_err(|_| "could not read random bytes")?;
    let mut check = [0u8; 4];
    provider.secure_random.fill(&mut check).map_err(|_| "could not read random bytes")?;
    from_seed(seed, u32::from_be_bytes(check))
}

/// Derive the public half from a seed. Separate from [`generate`] so a test can pin a
/// seed and compare the whole encoding.
pub fn from_seed(seed: [u8; 32], check: u32) -> Result<Ed25519Key, String> {
    use aws_lc_rs::signature::KeyPair;
    let pair = aws_lc_rs::signature::Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|e| format!("could not derive an Ed25519 key: {e}"))?;
    let mut public = [0u8; 32];
    public.copy_from_slice(pair.public_key().as_ref());
    Ok(Ed25519Key { seed, public, check })
}

/// The `ssh-ed25519 AAAA...` line, which is what `send-ssh-public-key` takes and what
/// lands in the instance's `authorized_keys`.
pub fn authorized_key(key: &Ed25519Key) -> String {
    format!("ssh-ed25519 {}", base64ct::Base64::encode_string(&public_blob(key)))
}

/// The private key as OpenSSH reads it.
///
/// Unencrypted — cipher `none`, kdf `none` — because the key exists for the length of one
/// `ssh` invocation and is written to a file the command creates `0400` and deletes on
/// the way out. A passphrase would have to be typed, which defeats the point.
///
/// The base64 is emitted as **one line**, not wrapped at 70 characters the way
/// `ssh-keygen` writes it. That is what the reference produces, and OpenSSH's reader does
/// not care about line length.
pub fn private_pem(key: &Ed25519Key) -> String {
    let mut section = Vec::new();
    // The same value twice: OpenSSH compares them to tell a good passphrase from a bad
    // one. With no passphrase they are only a formality, but they are a required one.
    section.extend_from_slice(&key.check.to_be_bytes());
    section.extend_from_slice(&key.check.to_be_bytes());
    push_string(&mut section, b"ssh-ed25519");
    push_string(&mut section, &key.public);
    // The private string is the seed *followed by* the public key, 64 bytes in total.
    let mut private = Vec::with_capacity(64);
    private.extend_from_slice(&key.seed);
    private.extend_from_slice(&key.public);
    push_string(&mut section, &private);
    push_string(&mut section, b"");
    // Padded to the cipher's block size with 1, 2, 3... which for `none` is 8.
    let mut pad = 1u8;
    while section.len() % 8 != 0 {
        section.push(pad);
        pad += 1;
    }

    let mut blob = Vec::new();
    blob.extend_from_slice(b"openssh-key-v1\0");
    push_string(&mut blob, b"none");
    push_string(&mut blob, b"none");
    push_string(&mut blob, b"");
    blob.extend_from_slice(&1u32.to_be_bytes());
    push_string(&mut blob, &public_blob(key));
    push_string(&mut blob, &section);

    format!(
        "-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n-----END OPENSSH PRIVATE KEY-----\n",
        base64ct::Base64::encode_string(&blob)
    )
}

/// The wire blob behind both formats: the key type, then the 32 public bytes.
fn public_blob(key: &Ed25519Key) -> Vec<u8> {
    let mut blob = Vec::new();
    push_string(&mut blob, b"ssh-ed25519");
    push_string(&mut blob, &key.public);
    blob
}

/// An SSH wire string: a four-byte big-endian length, then the bytes.
fn push_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8032 section 7.1's first Ed25519 test vector: this seed has this public key.
    /// It pins the derivation, without which every other test here would agree with
    /// itself and with nothing else.
    #[test]
    fn the_public_half_matches_the_rfc_8032_vector() {
        let seed = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03,
            0x1c, 0xae, 0x7f, 0x60,
        ];
        let key = from_seed(seed, 0).expect("derives");
        assert_eq!(
            key.public,
            [
                0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64,
                0x07, 0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68,
                0xf7, 0x07, 0x51, 0x1a,
            ]
        );
    }

    /// The authorized-keys line is the type, a space, and the base64 blob — and the blob
    /// always starts with the same 15 bytes, because it opens with the length-prefixed
    /// string `ssh-ed25519`.
    #[test]
    fn the_authorized_key_line_has_the_shape_openssh_writes() {
        let key = from_seed([7u8; 32], 0).expect("derives");
        let line = authorized_key(&key);
        assert!(line.starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI"), "{line}");
        assert_eq!(line.split(' ').count(), 2, "{line}");
    }

    /// The container's framing: magic, both `none`s, one key, and a private section
    /// padded to a multiple of 8.
    #[test]
    fn the_private_container_is_framed_the_way_openssh_expects() {
        let key = from_seed([3u8; 32], 0x0102_0304).expect("derives");
        let pem = private_pem(&key);
        let body: String = pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect();
        let blob = base64ct::Base64::decode_vec(&body).expect("decodes");
        assert_eq!(&blob[..15], b"openssh-key-v1\0");
        let mut offset = 15;
        fn take(blob: &[u8], offset: &mut usize) -> Vec<u8> {
            let length =
                u32::from_be_bytes(blob[*offset..*offset + 4].try_into().expect("4")) as usize;
            let value = blob[*offset + 4..*offset + 4 + length].to_vec();
            *offset += 4 + length;
            value
        }
        assert_eq!(take(&blob, &mut offset), b"none");
        assert_eq!(take(&blob, &mut offset), b"none");
        assert_eq!(take(&blob, &mut offset), b"");
        let keys = u32::from_be_bytes(blob[offset..offset + 4].try_into().expect("4"));
        assert_eq!(keys, 1);
        offset += 4;
        assert_eq!(take(&blob, &mut offset), public_blob(&key));
        let section = take(&blob, &mut offset);
        assert_eq!(section.len() % 8, 0, "the private section must be block-aligned");
        assert_eq!(&section[..4], &0x0102_0304u32.to_be_bytes());
        assert_eq!(&section[4..8], &0x0102_0304u32.to_be_bytes());
        assert_eq!(offset, blob.len(), "nothing may follow the private section");
    }

    /// The test that actually settles it: OpenSSH's own parser reads our private file and
    /// derives the same public key we emit. Nothing about the container can be wrong and
    /// still pass this.
    ///
    /// Skipped where `ssh-keygen` is not installed rather than failing, since it is not a
    /// build dependency.
    #[test]
    fn ssh_keygen_reads_our_private_key_and_agrees_on_the_public_one() {
        let Ok(found) = std::process::Command::new("ssh-keygen").arg("-?").output() else {
            eprintln!("skipping: ssh-keygen is not installed");
            return;
        };
        let _ = found;
        let key = from_seed([42u8; 32], 0xDEAD_BEEF).expect("derives");
        let path = std::env::temp_dir()
            .join(format!("awsc-sshkey-test-{}-{:?}", std::process::id(), std::thread::current().id()));
        std::fs::write(&path, private_pem(&key)).expect("writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("chmods");
        }
        let output = std::process::Command::new("ssh-keygen")
            .arg("-y")
            .arg("-f")
            .arg(&path)
            .output()
            .expect("runs ssh-keygen");
        let _ = std::fs::remove_file(&path);
        assert!(
            output.status.success(),
            "ssh-keygen rejected the key: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), authorized_key(&key));
    }
}
