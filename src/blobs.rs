//! Attachment files, stored by the SHA-256 of their bytes.
//!
//! Task 5 fills this in (streaming writes, the size limit, verified writes
//! for sync). For now it only answers whether a blob is held, which is what
//! `PostJson.attachment.available` reports.

use std::path::{Path, PathBuf};

/// A directory of blobs, one file per SHA-256 (lowercase hex).
#[derive(Debug, Clone)]
pub struct Blobs {
    dir: PathBuf,
}

/// Whether `s` is a SHA-256 in lowercase hex, the only name a blob file has.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Blobs {
    /// The blob store in `dir`. Does not touch the disk.
    pub fn new(dir: impl Into<PathBuf>) -> Blobs {
        Blobs { dir: dir.into() }
    }

    /// The directory the blobs live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where the blob `sha` is (or would be) stored. `sha` must pass
    /// [`is_sha256`]: it becomes a file name.
    pub fn path(&self, sha: &str) -> PathBuf {
        self.dir.join(sha)
    }

    /// Whether the bytes for `sha` are held here.
    pub fn has(&self, sha: &str) -> bool {
        is_sha256(sha) && self.path(sha).is_file()
    }
}
