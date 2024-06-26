//! Blocking `pxar` format handling.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::decoder::{self, SeqRead};
use crate::util::poll_result_once;
use crate::{Entry, PxarVariant};

/// Blocking `pxar` decoder.
///
/// This is the blocking I/O version of the `pxar` decoder. This will *not* work with an
/// asynchronous I/O object. I/O must always return `Poll::Ready`.
///
/// Attempting to use a `Waker` from this context *will* `panic!`
///
/// If you need to use asynchronous I/O, use `aio::Decoder`.
#[repr(transparent)]
pub struct Decoder<T> {
    inner: decoder::DecoderImpl<T>,
}

impl<T: io::Read> Decoder<StandardReader<T>> {
    /// Decode a `pxar` archive from a regular `std::io::Read` input.
    #[inline]
    pub fn from_std(input: PxarVariant<T, T>) -> io::Result<Self> {
        Decoder::new(input.wrap(|i| StandardReader::new(i)))
    }

    /// Get a direct reference to the reader contained inside the contained [`StandardReader`].
    pub fn input(&mut self) -> &T {
        self.inner.input().inner()
    }
}

impl Decoder<StandardReader<std::fs::File>> {
    /// Convenience shortcut for `File::open` followed by `Accessor::from_file`.
    pub fn open<P: AsRef<Path>>(path: PxarVariant<P, P>) -> io::Result<Self> {
        let input = match path {
            PxarVariant::Split(input, payload_input) => PxarVariant::Split(
                std::fs::File::open(input)?,
                std::fs::File::open(payload_input)?,
            ),
            PxarVariant::Unified(input) => PxarVariant::Unified(std::fs::File::open(input)?),
        };
        Self::from_std(input)
    }
}

impl<T: SeqRead> Decoder<T> {
    /// Create a *blocking* decoder from an input implementing our internal read interface.
    ///
    /// Note that the `input`'s `SeqRead` implementation must always return `Poll::Ready` and is
    /// not allowed to use the `Waker`, as this will cause a `panic!`.
    /// The optional payload input must be used to restore regular file payloads for payload references
    /// encountered within the archive.
    pub fn new(input: PxarVariant<T, T>) -> io::Result<Self> {
        Ok(Self {
            inner: poll_result_once(decoder::DecoderImpl::new(input))?,
        })
    }

    /// Internal helper for `Accessor`. In this case we have the low-level state machine, and the
    /// layer "above" the `Accessor` propagates the actual type (sync vs async).
    pub(crate) fn from_impl(inner: decoder::DecoderImpl<T>) -> Self {
        Self { inner }
    }

    // I would normally agree with clippy, but this here is to be consistent with the async
    // counterpart, and we *do* implement Iterator as well, so that's fine!
    #[allow(clippy::should_implement_trait)]
    /// If this is a directory entry, get the next item inside the directory.
    pub fn next(&mut self) -> Option<io::Result<Entry>> {
        poll_result_once(self.inner.next_do()).transpose()
    }

    /// Get a reader for the contents of the current entry, if the entry has contents.
    pub fn contents(&mut self) -> io::Result<Option<Contents<T>>> {
        let content_reader = poll_result_once(self.inner.content_reader())?;
        Ok(content_reader.map(|inner| Contents { inner }))
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

impl<T: SeqRead> Iterator for Decoder<T> {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        Decoder::next(self)
    }
}

/// Pxar decoder read adapter for `std::io::Read`.
pub struct StandardReader<T> {
    inner: T,
}

impl<T: io::Read> StandardReader<T> {
    /// Make a new [`StandardReader`].
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Get an immutable reference to the contained reader.
    pub fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T: io::Read> SeqRead for StandardReader<T> {
    fn poll_seq_read(
        self: Pin<&mut Self>,
        _cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(unsafe { self.get_unchecked_mut() }.inner.read(buf))
    }
}

/// Reader for file contents inside a pxar archive.
pub struct Contents<'a, T: SeqRead> {
    inner: decoder::Contents<'a, T>,
}

impl<'a, T: SeqRead> io::Read for Contents<'a, T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        poll_result_once(super::seq_read(&mut self.inner, buf))
    }
}
