//! Asynchronous `pxar` format handling.

use std::io;

#[cfg(feature = "tokio-fs")]
use std::path::Path;

use crate::decoder::{self, SeqRead};
use crate::Entry;

/// Asynchronous `pxar` decoder.
///
/// This is the `async` version of the `pxar` decoder.
#[repr(transparent)]
pub struct Decoder<T> {
    inner: decoder::DecoderImpl<T>,
}

#[cfg(feature = "futures-io")]
impl<T: futures::io::AsyncRead> Decoder<FuturesReader<T>> {
    /// Decode a `pxar` archive from a `futures::io::AsyncRead` input.
    #[inline]
    pub async fn from_futures(input: T) -> io::Result<Self> {
        Decoder::new(FuturesReader::new(input)).await
    }
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

    /// Include goodbye tables in iteration.
    pub fn enable_goodbye_entries(&mut self, on: bool) {
        self.inner.with_goodbye_tables = on;
    }

    /// Turn this decoder into a `Stream`.
    #[cfg(feature = "futures-io")]
    pub fn into_stream(self) -> DecoderStream<T> {
        DecoderStream::new(self)
    }
}

#[cfg(feature = "futures-io")]
mod stream {
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use super::{Entry, SeqRead};

    /// A wrapper for the async decoder implementing `futures::stream::Stream`.
    ///
    /// As long as streams are poll-based this wrapper is required to turn `async fn next()` into
    /// `Stream`'s `poll_next()` interface.
    #[allow(clippy::type_complexity)] // yeah no
    pub struct DecoderStream<T> {
        inner: super::Decoder<T>,
        future: Option<Pin<Box<dyn Future<Output = Option<io::Result<Entry>>>>>>,
    }

    impl<T> DecoderStream<T> {
        pub fn new(inner: super::Decoder<T>) -> Self {
            Self {
                inner,
                future: None,
            }
        }
    }

    impl<T: SeqRead> futures::stream::Stream for DecoderStream<T> {
        type Item = io::Result<Entry>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
            let this = unsafe { self.get_unchecked_mut() };
            loop {
                if let Some(mut fut) = this.future.take() {
                    match fut.as_mut().poll(cx) {
                        Poll::Ready(res) => return Poll::Ready(res),
                        Poll::Pending => {
                            this.future = Some(fut);
                            return Poll::Pending;
                        }
                    }
                }
                unsafe {
                    let fut: Box<dyn Future<Output = _>> = Box::new(this.inner.next());
                    // Discard the lifetime:
                    let fut: *mut (dyn Future<Output = Option<io::Result<Entry>>> + 'static) =
                        core::mem::transmute(Box::into_raw(fut));
                    let fut = Box::from_raw(fut);
                    this.future = Some(Pin::new_unchecked(fut));
                }
            }
        }
    }
}

#[cfg(feature = "futures-io")]
pub use stream::DecoderStream;

#[cfg(feature = "futures-io")]
mod fut {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Read adapter for `futures::io::AsyncRead`
    pub struct FuturesReader<T> {
        inner: T,
    }

    impl<T: futures::io::AsyncRead> FuturesReader<T> {
        pub fn new(inner: T) -> Self {
            Self { inner }
        }
    }

    impl<T: futures::io::AsyncRead> crate::decoder::SeqRead for FuturesReader<T> {
        fn poll_seq_read(
            self: Pin<&mut Self>,
            cx: &mut Context,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            unsafe {
                self.map_unchecked_mut(|this| &mut this.inner)
                    .poll_read(cx, buf)
            }
        }
    }
}

#[cfg(feature = "futures-io")]
use fut::FuturesReader;

#[cfg(feature = "tokio-io")]
mod tok {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Read adapter for `futures::io::AsyncRead`
    pub struct TokioReader<T> {
        inner: T,
    }

    impl<T: tokio::io::AsyncRead> TokioReader<T> {
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
}

#[cfg(feature = "tokio-io")]
use tok::TokioReader;
