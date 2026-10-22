//! Attachment files, stored by the SHA-256 of their bytes.
//!
//! A blob is the file `<dir>/<sha256, lowercase hex>`. Bytes are hashed while
//! they stream into a temp file beside the blobs (`.tmp-<ulid>`, never a
//! valid blob name) and only a complete, checked file is renamed to its hash,
//! so a file under a hash name is always whole and always has that hash. A
//! temp file is removed on every way out: an error, the size limit, a request
//! dropped because its client hung up, or a hash that does not match.
//!
//! Nothing here holds a whole file in memory. The one place a name from
//! outside becomes a path is [`Blobs::path`], and it only accepts hashes.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use ulid::Ulid;

use crate::error::Error;

/// Temp files start with this; no hash does.
const TEMP_PREFIX: &str = ".tmp-";
/// What [`Blobs::path`] answers for a name that is not a hash: a file that
/// is never there.
const NOT_A_BLOB: &str = ".invalid";
const MIB: u64 = 1024 * 1024;

/// A directory of blobs, one file per SHA-256 (lowercase hex).
#[derive(Debug, Clone)]
pub struct Blobs {
    dir: PathBuf,
}

/// Whether `s` is a SHA-256 in lowercase hex, the only name a blob file has.
pub fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Why [`Blobs::put_stream`] stored nothing.
#[derive(Debug)]
pub enum PutError {
    /// The stream held more than `max_bytes`. The rest of it was not read.
    TooLarge { max_bytes: u64 },
    /// The stream broke, or the disk failed.
    Failed(Error),
}

impl fmt::Display for PutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PutError::TooLarge { max_bytes } if max_bytes % MIB == 0 => {
                write!(f, "attachment is over {} MB", max_bytes / MIB)
            }
            PutError::TooLarge { max_bytes } => {
                write!(f, "attachment is over {max_bytes} bytes")
            }
            PutError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for PutError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            PutError::TooLarge { .. } => None,
            PutError::Failed(e) => Some(e),
        }
    }
}

impl From<Error> for PutError {
    fn from(e: Error) -> Self {
        PutError::Failed(e)
    }
}

/// Too large is [`Error::Usage`] ("attachment is over N MB").
impl From<PutError> for Error {
    fn from(e: PutError) -> Self {
        match e {
            PutError::TooLarge { .. } => Error::Usage(e.to_string()),
            PutError::Failed(e) => e,
        }
    }
}

fn io_error(what: &'static str) -> impl Fn(std::io::Error) -> Error {
    move |e| Error::Internal(anyhow!("{what}: {e}"))
}

/// A file being written. It is removed when dropped, unless it has been
/// renamed away by then (removing a file that is gone is not an error).
/// Because a dropped future runs no more code, this is what cleans up after a
/// client that disconnects mid-upload.
struct TempFile {
    path: PathBuf,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A whole stream, on disk in a temp file and hashed.
struct Received {
    temp: TempFile,
    sha: String,
    len: u64,
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    out
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

    /// Where the blob `sha` is (or would be) stored. Anything that is not a
    /// hash (see [`is_sha256`]) gets a path inside the directory that never
    /// exists, so a name from outside cannot point anywhere else.
    pub fn path(&self, sha: &str) -> PathBuf {
        if is_sha256(sha) {
            self.dir.join(sha)
        } else {
            self.dir.join(NOT_A_BLOB)
        }
    }

    /// Whether the bytes for `sha` are held here.
    pub fn has(&self, sha: &str) -> bool {
        is_sha256(sha) && self.path(sha).is_file()
    }

    /// Opens the blob `sha` for reading, with its length. `None` if it is
    /// not held (or `sha` is not a hash).
    pub async fn open(&self, sha: &str) -> Result<Option<(tokio::fs::File, u64)>, Error> {
        if !is_sha256(sha) {
            return Ok(None);
        }
        let file = match tokio::fs::File::open(self.path(sha)).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_error("cannot open attachment")(e)),
        };
        let meta = file
            .metadata()
            .await
            .map_err(io_error("cannot read attachment"))?;
        Ok(Some((file, meta.len())))
    }

    /// Stores a stream of bytes under its SHA-256 and returns the hash and the
    /// length. The bytes go to disk as they arrive, never all in memory, and
    /// the stream is not read past `max_bytes`: [`PutError::TooLarge`] (its
    /// message is `attachment is over N MB`). If the same bytes are already
    /// here, nothing changes. Whatever happens, no partial file is left.
    pub async fn put_stream<S, E>(
        &self,
        stream: S,
        max_bytes: u64,
    ) -> Result<(String, u64), PutError>
    where
        S: Stream<Item = Result<Bytes, E>>,
        E: Into<PutError>,
    {
        let received = self.receive(stream, max_bytes).await?;
        let (sha, len) = (received.sha.clone(), received.len);
        self.install(received).await?;
        Ok((sha, len))
    }

    /// Stores a stream of bytes as the blob `sha`, which is what sync does
    /// with a file a peer announced. The bytes are only kept if they hash to
    /// `sha` ([`Error::Usage`] otherwise, and nothing is stored). A blob that
    /// is already here is left alone without reading the stream.
    ///
    /// The stream is not length-limited: a caller that knows how big the file
    /// should be bounds the stream itself.
    pub async fn put_verified<S, E>(&self, sha: &str, stream: S) -> Result<(), Error>
    where
        S: Stream<Item = Result<Bytes, E>>,
        E: Into<Error>,
    {
        if !is_sha256(sha) {
            return Err(Error::Usage(format!(
                "{sha:?} is not a sha256: expected 64 lowercase hex digits"
            )));
        }
        if self.has(sha) {
            return Ok(());
        }
        let stream = stream.map(|chunk| chunk.map_err(|e| PutError::Failed(e.into())));
        let received = self.receive(stream, u64::MAX).await?;
        if received.sha != sha {
            return Err(Error::Usage(format!(
                "the bytes received for {sha} hash to {}",
                received.sha
            )));
        }
        self.install(received).await
    }

    /// Removes temp files a previous run left behind (a process killed
    /// mid-upload). Only call it when no upload is under way, such as at
    /// start-up. Blobs are not touched.
    pub fn clear_temp(&self) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let is_temp = entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(TEMP_PREFIX));
            if is_temp {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// Writes `stream` to a new temp file while hashing it.
    async fn receive<S, E>(&self, stream: S, max_bytes: u64) -> Result<Received, PutError>
    where
        S: Stream<Item = Result<Bytes, E>>,
        E: Into<PutError>,
    {
        let path = self.dir.join(format!("{TEMP_PREFIX}{}", Ulid::generate()));
        // Created without an await, and handed to a guard at once, so there is
        // no point at which the file exists and nothing owns its removal.
        let file = std::fs::File::create(&path).map_err(io_error("cannot create attachment"))?;
        let temp = TempFile { path };
        let mut file = tokio::fs::File::from_std(file);

        let mut hasher = Sha256::new();
        let mut len: u64 = 0;
        let mut stream = std::pin::pin!(stream);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(Into::<PutError>::into)?;
            len = len.saturating_add(chunk.len() as u64);
            if len > max_bytes {
                return Err(PutError::TooLarge { max_bytes });
            }
            hasher.update(&chunk);
            file.write_all(&chunk)
                .await
                .map_err(io_error("cannot write attachment"))?;
        }
        file.flush()
            .await
            .map_err(io_error("cannot write attachment"))?;
        // On disk before it gets its final name: a crash must not leave a
        // short file under the name of a hash.
        file.sync_all()
            .await
            .map_err(io_error("cannot write attachment"))?;
        Ok(Received {
            temp,
            sha: hex(&hasher.finalize()),
            len,
        })
    }

    /// Moves a received file to its hash name. Runs to the end on a blocking
    /// thread even if the caller is dropped meanwhile; either way the temp
    /// file is gone afterwards.
    async fn install(&self, received: Received) -> Result<(), Error> {
        let target = self.path(&received.sha);
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || {
            let Received { temp, .. } = received;
            // Identical bytes already here: keep that file, drop this copy.
            if target.is_file() {
                return Ok(());
            }
            std::fs::rename(&temp.path, &target).map_err(io_error("cannot store attachment"))?;
            // Make the new name survive a crash. Failing to is not an error:
            // the file is there, and sync fetches it again if it is not.
            if let Ok(dir) = std::fs::File::open(dir) {
                let _ = dir.sync_all();
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Internal(anyhow!("attachment task failed: {e}")))?
    }
}
