//! Blocking `pxar` random access handling.

use std::io;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::accessor::{self, ReadAt};
use crate::decoder::Decoder;
use crate::util::poll_result_once;
use crate::Entry;

/// Blocking `pxar` random-access decoder.
///
/// This is the blocking I/O version of the `pxar` accessor. This will *not* work with an
/// asynchronous I/O object. I/O must always return `Poll::Ready`.
///
/// Attempting to use a `Waker` from this context *will* `panic!`
///
/// If you need to use asynchronous I/O, use `aio::Accessor`.
#[repr(transparent)]
pub struct Accessor<T> {
    inner: accessor::AccessorImpl<T>,
}

impl<T: FileExt> Accessor<T> {
    /// Decode a `pxar` archive from a standard file implementing `FileExt`.
    #[inline]
    pub fn from_file_and_size(input: T, size: u64) -> io::Result<Accessor<FileReader<T>>> {
        Accessor::new(FileReader::new(input), size)
    }
}

impl Accessor<FileReader<std::fs::File>> {
    /// Decode a `pxar` archive from a regular `std::io::File` input.
    #[inline]
    pub fn from_file(input: std::fs::File) -> io::Result<Self> {
        let size = input.metadata()?.len();
        Accessor::from_file_and_size(input, size)
    }

    /// Convenience shortcut for `File::open` followed by `Accessor::from_file`.
    pub fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::from_file(std::fs::File::open(path.as_ref())?)
    }
}

impl<T: ReadAt> Accessor<T> {
    /// Create a *blocking* random-access decoder from an input implementing our internal read
    /// interface.
    ///
    /// Note that the `input`'s `SeqRead` implementation must always return `Poll::Ready` and is
    /// not allowed to use the `Waker`, as this will cause a `panic!`.
    pub fn new(input: T, size: u64) -> io::Result<Self> {
        Ok(Self {
            inner: poll_result_once(accessor::AccessorImpl::new(input, size))?,
        })
    }

    /// Open a directory handle to the root of the pxar archive.
    pub fn open_root<'a>(&'a self) -> io::Result<Directory<'a>> {
        Ok(Directory::new(poll_result_once(self.inner.open_root())?))
    }
}

/// Adapter for FileExt readers.
pub struct FileReader<T> {
    inner: T,
}

impl<T: FileExt> FileReader<T> {
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T: FileExt> ReadAt for FileReader<T> {
    fn poll_read_at(
        self: Pin<&Self>,
        _cx: &mut Context,
        buf: &mut [u8],
        offset: u64,
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(self.get_ref().inner.read_at(buf, offset))
    }
}

/// Blocking Directory variant:
#[repr(transparent)]
pub struct Directory<'a> {
    inner: accessor::DirectoryImpl<'a>,
}

impl<'a> Directory<'a> {
    fn new(inner: accessor::DirectoryImpl<'a>) -> Self {
        Self { inner }
    }

    /// Get a decoder for the directory contents.
    pub fn decode_full(&self) -> io::Result<Decoder<accessor::SeqReadAtAdapter<'a>>> {
        Ok(Decoder::from_impl(poll_result_once(
            self.inner.decode_full(),
        )?))
    }

    /// Lookup an entry in a directory.
    pub fn lookup<P: AsRef<Path>>(&'a self, path: P) -> io::Result<Option<FileEntry<'a>>> {
        if let Some(file_entry) = poll_result_once(self.inner.lookup(path.as_ref()))? {
            Ok(Some(FileEntry { inner: file_entry }))
        } else {
            Ok(None)
        }
    }

    /// Get an iterator over the directory's contents.
    pub fn read_dir(&'a self) -> ReadDir<'a> {
        ReadDir {
            inner: self.inner.read_dir(),
        }
    }
}

/// A file entry retrieved from a `Directory` via the `lookup` method.
#[repr(transparent)]
pub struct FileEntry<'a> {
    inner: accessor::FileEntryImpl<'a>,
}

impl<'a> FileEntry<'a> {
    pub fn enter_directory(&self) -> io::Result<Directory<'a>> {
        Ok(Directory::new(poll_result_once(
            self.inner.enter_directory(),
        )?))
    }

    #[inline]
    pub fn into_entry(self) -> Entry {
        self.inner.into_entry()
    }

    #[inline]
    pub fn entry(&self) -> &Entry {
        &self.inner.entry()
    }
}

impl<'a> std::ops::Deref for FileEntry<'a> {
    type Target = Entry;

    fn deref(&self) -> &Self::Target {
        self.entry()
    }
}

/// An iterator over the contents of a `Directory`.
#[repr(transparent)]
pub struct ReadDir<'a> {
    inner: accessor::ReadDirImpl<'a>,
}

impl<'a> Iterator for ReadDir<'a> {
    type Item = io::Result<DirEntry<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        match poll_result_once(self.inner.next()) {
            Ok(Some(inner)) => Some(Ok(DirEntry { inner })),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        }
    }
}

impl<'a> std::iter::FusedIterator for ReadDir<'a> {}

/// A directory entry. When iterating through the contents of a directory we first get access to
/// the file name. The remaining information can be decoded afterwards.
#[repr(transparent)]
pub struct DirEntry<'a> {
    inner: accessor::DirEntryImpl<'a>,
}

impl<'a> DirEntry<'a> {
    pub fn file_name(&self) -> &Path {
        self.inner.file_name()
    }

    pub fn get_entry(&self) -> io::Result<FileEntry<'a>> {
        poll_result_once(self.inner.get_entry()).map(|inner| FileEntry { inner })
    }
}
