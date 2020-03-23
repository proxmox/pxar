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

    /// If this is a directory entry, get the next item inside the directory.
    pub async fn next(&mut self) -> Option<io::Result<Entry>> {
        self.inner.next_do().await.transpose()
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

macro_rules! async_io_impl {
    (
        #[cfg( $($attr:tt)+ )]
        mod $mod:ident {
            $(#[$docs:meta])*
            $name:ident : $trait:path ;
        }
    ) => {
        #[cfg( $($attr)+ )]
        mod $mod {
            use std::io;
            use std::pin::Pin;
            use std::task::{Context, Poll};

            $(#[$docs])*
            pub struct $name<T> {
                inner: T,
            }

            impl<T: $trait> $name<T> {
                pub fn new(inner: T) -> Self {
                    Self { inner }
                }
            }

            impl<T: $trait> crate::decoder::SeqRead for $name<T> {
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
        #[cfg( $($attr)+ )]
        pub use $mod::$name;
    }
}

async_io_impl! {
    #[cfg(feature = "futures-io")]
    mod fut {
        /// Read adapter for `futures::io::AsyncRead`.
        FuturesReader : futures::io::AsyncRead;
    }
}

async_io_impl! {
    #[cfg(feature = "tokio-io")]
    mod tok {
        /// Read adapter for `tokio::io::AsyncRead`.
        TokioReader : tokio::io::AsyncRead;
    }
}
