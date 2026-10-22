//! A reader that insists on an exact length.
//!
//! `./beacon attach` streams a host file into the container on stdin. If the
//! `docker compose exec` client dies on the way, stdin just ends, and an upload
//! that does not know how long the file should be would post what it got as if
//! it were the whole file. [`Exact`] wraps the file and fails the read instead,
//! which makes the upload abort before its multipart body is complete, so the
//! node creates no post. It also fails if the file turns out to be longer
//! (it grew after its size was taken).

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

/// How a stream turned out not to have the length it was promised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    /// The stream ended after `read` bytes, short of the size.
    Early { read: u64 },
    /// The stream has more bytes than the size.
    Long,
}

impl Mismatch {
    /// What to tell the user about a stream promised to be `size` bytes.
    pub fn explain(self, size: u64) -> String {
        match self {
            Mismatch::Early { read } => {
                format!("the file ended early (read {read} of {size} bytes); nothing was posted")
            }
            Mismatch::Long => {
                format!("the file ran longer than {size} bytes; nothing was posted")
            }
        }
    }

    fn io_error(self, size: u64) -> io::Error {
        let kind = match self {
            Mismatch::Early { .. } => io::ErrorKind::UnexpectedEof,
            Mismatch::Long => io::ErrorKind::InvalidData,
        };
        io::Error::new(kind, self.explain(size))
    }
}

/// Where an [`Exact`] reader leaves its verdict, for whoever holds the upload
/// that the reader's error brought down (which reports it as a broken request).
#[derive(Debug, Clone, Default)]
pub struct Verdict(Arc<Mutex<Option<Mismatch>>>);

impl Verdict {
    /// How the stream failed to be the right length, if it did.
    pub fn mismatch(&self) -> Option<Mismatch> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set(&self, mismatch: Mismatch) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(mismatch);
    }
}

/// `inner`, which must deliver exactly `size` bytes and then end.
pub struct Exact<R> {
    inner: R,
    size: u64,
    read: u64,
    verdict: Verdict,
}

impl<R> Exact<R> {
    pub fn new(inner: R, size: u64, verdict: Verdict) -> Exact<R> {
        Exact {
            inner,
            size,
            read: 0,
            verdict,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for Exact<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        // Once failed, always failed: the upload must not go on after it.
        if let Some(mismatch) = this.verdict.mismatch() {
            return Poll::Ready(Err(mismatch.io_error(this.size)));
        }
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        // Ask for one byte more than is left, so a stream that is too long
        // shows it, and let that byte (never passed on) be the proof.
        let left = this.size - this.read;
        let want = usize::try_from(left.saturating_add(1))
            .unwrap_or(usize::MAX)
            .min(buf.remaining());
        let got = {
            let mut limited = ReadBuf::new(buf.initialize_unfilled_to(want));
            ready!(Pin::new(&mut this.inner).poll_read(cx, &mut limited))?;
            limited.filled().len()
        };
        let mismatch = if got as u64 > left {
            Some(Mismatch::Long)
        } else if got == 0 && this.read < this.size {
            Some(Mismatch::Early { read: this.read })
        } else {
            None
        };
        if let Some(mismatch) = mismatch {
            this.verdict.set(mismatch);
            return Poll::Ready(Err(mismatch.io_error(this.size)));
        }
        this.read += got as u64;
        buf.advance(got);
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    async fn drain(bytes: &[u8], size: u64) -> (io::Result<Vec<u8>>, Option<Mismatch>) {
        let verdict = Verdict::default();
        let mut reader = Exact::new(bytes, size, verdict.clone());
        let mut got = Vec::new();
        let result = reader.read_to_end(&mut got).await.map(|_| got);
        (result, verdict.mismatch())
    }

    #[tokio::test]
    async fn the_right_length_passes_untouched() {
        for len in [0usize, 1, 5000] {
            let bytes = vec![7u8; len];
            let (got, mismatch) = drain(&bytes, len as u64).await;
            assert_eq!(got.unwrap(), bytes);
            assert_eq!(mismatch, None);
        }
    }

    #[tokio::test]
    async fn short_and_long_streams_fail_with_the_count() {
        let (got, mismatch) = drain(b"abc", 10).await;
        assert_eq!(got.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(mismatch, Some(Mismatch::Early { read: 3 }));
        let (got, mismatch) = drain(b"abcdef", 5).await;
        assert_eq!(got.unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(mismatch, Some(Mismatch::Long));
        assert_eq!(
            Mismatch::Early { read: 3 }.explain(10),
            "the file ended early (read 3 of 10 bytes); nothing was posted"
        );
    }
}
