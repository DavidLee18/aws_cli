//! Request bodies that do not have to fit in memory.
//!
//! The old transport carried every body as a `Vec<u8>`, which meant a 5 GB upload was a
//! 5 GB allocation and each retry attempt cloned it. Uploads are now described rather
//! than materialised: [`Body::FileRange`] names a slice of a file and is streamed from
//! disk at send time, so a multipart part costs a 64 KiB read buffer regardless of how
//! big the part is, and a retry re-reads rather than re-copies.

use bytes::Bytes;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Body {
    Empty,
    /// An in-memory body. Cheap to clone across retries: `Bytes` is refcounted.
    Bytes(Bytes),
    /// A byte range of a file, read lazily while the request is being sent.
    FileRange { path: PathBuf, offset: u64, len: u64 },
}

impl Default for Body {
    fn default() -> Self {
        Body::Empty
    }
}

impl Body {
    pub fn from_vec(bytes: Vec<u8>) -> Body {
        if bytes.is_empty() {
            Body::Empty
        } else {
            Body::Bytes(Bytes::from(bytes))
        }
    }

    /// Content-Length for the request. Always known — AWS SigV4 has no use for chunked
    /// transfer encoding on the paths this client takes.
    pub fn len(&self) -> u64 {
        match self {
            Body::Empty => 0,
            Body::Bytes(b) => b.len() as u64,
            Body::FileRange { len, .. } => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The in-memory bytes, when there are any. `None` for a file-backed body, which is
    /// the signal that the payload cannot be hashed without reading the file.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Body::Empty => Some(&[]),
            Body::Bytes(b) => Some(b),
            Body::FileRange { .. } => None,
        }
    }
}
