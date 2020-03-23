//! Blocking `pxar` format handling.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::decoder::{self, SeqRead};
use crate::util::poll_result_once;
use crate::Entry;

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
    pub fn from_std(input: T) -> io::Result<Self> {
        Decoder::new(StandardReader::new(input))
    }
}

impl Decoder<StandardReader<std::fs::File>> {
    /// Convenience shortcut for `File::open` followed by `Accessor::from_file`.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::from_std(std::fs::File::open(path.as_ref())?)
    }
}

impl<T: SeqRead> Decoder<T> {
    /// Create a *blocking* decoder from an input implementing our internal read interface.
    ///
    /// Note that the `input`'s `SeqRead` implementation must always return `Poll::Ready` and is
    /// not allowed to use the `Waker`, as this will cause a `panic!`.
    pub fn new(input: T) -> io::Result<Self> {
        Ok(Self {
            inner: poll_result_once(decoder::DecoderImpl::new(input))?,
        })
    }

    /// Internal helper for `Accessor`. In this case we have the low-level state machine, and the
    /// layer "above" the `Accessor` propagates the actual type (sync vs async).
    pub(crate) fn from_impl(inner: decoder::DecoderImpl<T>) -> Self {
        Self { inner }
    }

    /// If this is a directory entry, get the next item inside the directory.
    pub fn next(&mut self) -> Option<io::Result<Entry>> {
        poll_result_once(self.inner.next_do()).transpose()
    }

    /// Get a reader for the contents of the current entry, if the entry has contents.
    pub fn contents(&mut self) -> Option<Contents> {
        self.inner.content_reader().map(|inner| Contents { inner })
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
    pub fn new(inner: T) -> Self {
        Self { inner }
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

pub struct Contents<'a> {
    inner: decoder::Contents<'a>,
}

impl<'a> io::Read for Contents<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        poll_result_once((&mut self.inner as &mut dyn SeqRead).seq_read(buf))
    }
}
