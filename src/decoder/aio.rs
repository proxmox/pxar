//! Asynchronous `pxar` format handling.

use std::io;

#[cfg(feature = "tokio-fs")]
use std::path::Path;

use crate::decoder::{self, Contents, SeqRead};
use crate::Entry;

/// Asynchronous `pxar` decoder.
///
/// This is the `async` version of the `pxar` decoder.
#[repr(transparent)]
pub struct Decoder<T> {
    inner: decoder::DecoderImpl<T>,
}

#[cfg(feature = "tokio-io")]
impl<T: tokio::io::AsyncRead> Decoder<TokioReader<T>> {
    /// Decode a `pxar` archive from a `tokio::io::AsyncRead` input.
    #[inline]
    pub async fn from_tokio(input: T) -> io::Result<Self> {
        Decoder::new(TokioReader::new(input)).await
    }
}

#[cfg(feature = "tokio-fs")]
impl Decoder<TokioReader<tokio::fs::File>> {
    /// Decode a `pxar` archive from a `tokio::io::AsyncRead` input.
    #[inline]
    pub async fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Decoder::from_tokio(tokio::fs::File::open(path.as_ref()).await?).await
    }
}

impl<T: SeqRead> Decoder<T> {
    /// Create an async decoder from an input implementing our internal read interface.
    pub async fn new(input: T) -> io::Result<Self> {
        Ok(Self {
            inner: decoder::DecoderImpl::new(input).await?,
        })
    }

    /// Internal helper for `Accessor`. In this case we have the low-level state machine, and the
    /// layer "above" the `Accessor` propagates the actual type (sync vs async).
    pub(crate) fn from_impl(inner: decoder::DecoderImpl<T>) -> Self {
        Self { inner }
    }

    // I would normally agree with clippy, but this is async and we can at most implement Stream,
    // which we do with feature flags...
    #[allow(clippy::should_implement_trait)]
    /// If this is a directory entry, get the next item inside the directory.
    pub async fn next(&mut self) -> Option<io::Result<Entry>> {
        self.inner.next_do().await.transpose()
    }

    /// Get a reader for the contents of the current entry, if the entry has contents.
    pub fn contents(&mut self) -> Option<Contents<T>> {
        self.inner.content_reader()
    }

    /// Get the size of the current contents, if the entry has contents.
    pub fn content_size(&self) -> Option<u64> {
        self.inner.content_size()
    }

    /// Include goodbye tables in iteration.
    pub fn enable_goodbye_entries(&mut self, on: bool) {
        self.inner.with_goodbye_tables = on;
    }
}

#[cfg(feature = "tokio-io")]
mod tok {
    use crate::decoder::{Contents, SeqRead};
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Read adapter for `futures::io::AsyncRead`
    pub struct TokioReader<T> {
        inner: T,
    }

    impl<T: tokio::io::AsyncRead> TokioReader<T> {
        /// Create a new [TokioReader] from a type that implements [AsyncRead](tokio::io::AsyncRead).
        pub fn new(inner: T) -> Self {
            Self { inner }
        }
    }

    impl<T: tokio::io::AsyncRead> crate::decoder::SeqRead for TokioReader<T> {
        fn poll_seq_read(
            self: Pin<&mut Self>,
            cx: &mut Context,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            let mut read_buf = tokio::io::ReadBuf::new(buf);
            unsafe {
                self.map_unchecked_mut(|this| &mut this.inner)
                    .poll_read(cx, &mut read_buf)
                    .map_ok(|_| read_buf.filled().len())
            }
        }
    }

    impl<'a, T: crate::decoder::SeqRead> tokio::io::AsyncRead for Contents<'a, T> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            unsafe {
                // Safety: poll_seq_read will *probably* only write to the buffer, so we don't
                // initialize it first, instead we treat is a &[u8] immediately and uphold the
                // ReadBuf invariants in the conditional below.
                let write_buf =
                    &mut *(buf.unfilled_mut() as *mut [std::mem::MaybeUninit<u8>] as *mut [u8]);
                let result = self.poll_seq_read(cx, write_buf);
                if let Poll::Ready(Ok(n)) = result {
                    // if we've written data, advance both initialized and filled bytes cursor
                    buf.assume_init(n);
                    buf.advance(n);
                }
                result.map(|_| Ok(()))
            }
        }
    }
}

#[cfg(feature = "tokio-io")]
pub use tok::TokioReader;
