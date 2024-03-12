//! Asynchronous `pxar` random-access handling.
//!
//! Currently neither tokio nor futures have an `AsyncFileExt` variant.
//!
//! TODO: Implement a locking version for AsyncSeek+AsyncRead files?

use std::future::Future;
use std::io;
use std::mem;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::accessor::{self, cache::Cache, MaybeReady, ReadAt, ReadAtOperation};
use crate::decoder::aio::Decoder;
use crate::format::GoodbyeItem;
use crate::util;
use crate::{Entry, PxarVariant};

use super::sync::{FileReader, FileRefReader};

/// Asynchronous `pxar` random-access decoder.
///
///
/// This is the `async` version of the `pxar` accessor.
#[repr(transparent)]
pub struct Accessor<T> {
    inner: accessor::AccessorImpl<T>,
}

impl<T: FileExt> Accessor<FileReader<T>> {
    /// Decode a `pxar` archive from a standard file implementing `FileExt`.
    ///
    /// Note that "plain files" don't normally block on `read(2)` operations anyway, and tokio has
    /// no support for asynchronous `read_at` operations, so we allow creating an accessor backed
    /// by a blocking file.
    #[inline]
    pub async fn from_file_and_size(input: T, size: u64) -> io::Result<Self> {
        Accessor::new(PxarVariant::Unified(FileReader::new(input)), size).await
    }
}

impl Accessor<FileReader<std::fs::File>> {
    /// Decode a `pxar` archive from a regular `std::io::File` input.
    #[inline]
    pub async fn from_file(input: std::fs::File) -> io::Result<Self> {
        let size = input.metadata()?.len();
        Accessor::from_file_and_size(input, size).await
    }

    /// Convenience shortcut for `File::open` followed by `Accessor::from_file`.
    pub async fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        Self::from_file(std::fs::File::open(path.as_ref())?).await
    }
}

impl<T: Clone + std::ops::Deref<Target = std::fs::File>> Accessor<FileRefReader<T>> {
    /// Open an `Arc` or `Rc` of `File`.
    pub async fn from_file_ref(input: T) -> io::Result<Self> {
        let size = input.deref().metadata()?.len();
        Accessor::from_file_ref_and_size(input, size).await
    }
}

impl<T> Accessor<FileRefReader<T>>
where
    T: Clone + std::ops::Deref,
    T::Target: FileExt,
{
    /// Open an `Arc` or `Rc` of `File`.
    pub async fn from_file_ref_and_size(
        input: T,
        size: u64,
    ) -> io::Result<Accessor<FileRefReader<T>>> {
        Accessor::new(PxarVariant::Unified(FileRefReader::new(input)), size).await
    }
}

impl<T: ReadAt> Accessor<T> {
    /// Create a *blocking* random-access decoder from an input implementing our internal read
    /// interface.
    ///
    /// Note that the `input`'s `SeqRead` implementation must always return `Poll::Ready` and is
    /// not allowed to use the `Waker`, as this will cause a `panic!`.
    /// Optionally take the file payloads from the provided input stream rather than the regular
    /// pxar stream.
    pub async fn new(input: PxarVariant<T, (T, u64)>, size: u64) -> io::Result<Self> {
        Ok(Self {
            inner: accessor::AccessorImpl::new(input, size).await?,
        })
    }

    /// Open a directory handle to the root of the pxar archive.
    pub async fn open_root_ref(&self) -> io::Result<Directory<&dyn ReadAt>> {
        Ok(Directory::new(self.inner.open_root_ref().await?))
    }

    /// Set a cache for the goodbye tables to reduce random disk access.
    pub fn set_goodbye_table_cache<C>(&mut self, cache: Option<C>)
    where
        C: Cache<u64, [GoodbyeItem]> + Send + Sync + 'static,
    {
        self.inner
            .set_goodbye_table_cache(cache.map(|cache| Arc::new(cache) as _))
    }

    /// Get the full archive size we're allowed to access.
    #[inline]
    pub fn size(&self) -> u64 {
        self.inner.size()
    }
}

impl<T: Clone + ReadAt> Accessor<T> {
    /// Open the "root" directory entry of this pxar archive.
    pub async fn open_root(&self) -> io::Result<Directory<T>> {
        Ok(Directory::new(self.inner.open_root().await?))
    }

    /// Allow opening a directory at a specified offset.
    ///
    /// # Safety
    ///
    /// This should only be used with offsets known to point to the end of a directory, otherwise
    /// this usually fails, unless the data otherwise happens to look like a valid directory.
    pub async unsafe fn open_dir_at_end(&self, offset: u64) -> io::Result<Directory<T>> {
        Ok(Directory::new(unsafe {
            self.inner.open_dir_at_end(offset).await?
        }))
    }

    /// Allow opening a regular file from a specified range.
    ///
    /// # Safety
    ///
    /// Should only be used with `entry_range_info`s originating from the same archive, otherwise
    /// the result will be undefined and likely fail (or contain unexpected data).
    pub async unsafe fn open_file_at_range(
        &self,
        entry_range_info: &accessor::EntryRangeInfo,
    ) -> io::Result<FileEntry<T>> {
        Ok(FileEntry {
            inner: unsafe { self.inner.open_file_at_range(entry_range_info).await? },
        })
    }

    /// Allow opening arbitrary contents from a specific range.
    ///
    /// # Safety
    ///
    /// This will provide a reader over an arbitrary range of the archive file, so unless this
    /// comes from a actual file entry data, the contents might not make much sense.
    pub unsafe fn open_contents_at_range(&self, range: Range<u64>) -> FileContents<T> {
        FileContents {
            inner: unsafe { self.inner.open_contents_at_range(range) },
            at: 0,
            buffer: Vec::new(),
            future: None,
        }
    }

    /// Following a hardlink.
    pub async fn follow_hardlink(&self, entry: &FileEntry<T>) -> io::Result<FileEntry<T>> {
        Ok(FileEntry {
            inner: self.inner.follow_hardlink(&entry.inner).await?,
        })
    }
}

/// A pxar directory entry. This provdies blocking access to the contents of a directory.
#[repr(transparent)]
pub struct Directory<T> {
    inner: accessor::DirectoryImpl<T>,
}

impl<T: Clone + ReadAt> Directory<T> {
    fn new(inner: accessor::DirectoryImpl<T>) -> Self {
        Self { inner }
    }

    /// Get a decoder for the directory contents.
    pub async fn decode_full(&self) -> io::Result<Decoder<accessor::SeqReadAtAdapter<T>>> {
        Ok(Decoder::from_impl(self.inner.decode_full().await?))
    }

    /// Get a `FileEntry` referencing the directory itself.
    ///
    /// Helper function for our fuse implementation.
    pub async fn lookup_self(&self) -> io::Result<FileEntry<T>> {
        Ok(FileEntry {
            inner: self.inner.lookup_self().await?,
        })
    }

    /// Lookup an entry starting from this current directory.
    ///
    /// For convenience, this already resolves paths with multiple components.
    pub async fn lookup<P: AsRef<Path>>(&self, path: P) -> io::Result<Option<FileEntry<T>>> {
        if let Some(file_entry) = self.inner.lookup(path.as_ref()).await? {
            Ok(Some(FileEntry { inner: file_entry }))
        } else {
            Ok(None)
        }
    }

    /// Get an iterator over the directory's contents.
    pub fn read_dir(&self) -> ReadDir<T> {
        ReadDir {
            inner: self.inner.read_dir(),
        }
    }

    /// Get the number of entries in this directory.
    #[inline]
    pub fn entry_count(&self) -> usize {
        self.inner.entry_count()
    }
}

/// A file entry in a direcotry, retrieved via the `lookup` method or from
/// `DirEntry::decode_entry``.
#[derive(Clone)]
#[repr(transparent)]
pub struct FileEntry<T: Clone + ReadAt> {
    inner: accessor::FileEntryImpl<T>,
}

impl<T: Clone + ReadAt> FileEntry<T> {
    /// Get a handle to the subdirectory this file entry points to, if it is in fact a directory.
    pub async fn enter_directory(&self) -> io::Result<Directory<T>> {
        Ok(Directory::new(self.inner.enter_directory().await?))
    }

    /// For use with unsafe accessor methods.
    pub fn content_range(&self) -> io::Result<Option<Range<u64>>> {
        self.inner.content_range()
    }

    /// Get the file's contents.
    pub async fn contents(&self) -> io::Result<FileContents<T>> {
        Ok(FileContents {
            inner: self.inner.contents().await?,
            at: 0,
            buffer: Vec::new(),
            future: None,
        })
    }

    /// Convenience shortcut for when only the metadata contained in the [`Entry`] struct is of
    /// interest.
    #[inline]
    pub fn into_entry(self) -> Entry {
        self.inner.into_entry()
    }

    /// Access the contained [`Entry`] for metadata access.
    #[inline]
    pub fn entry(&self) -> &Entry {
        self.inner.entry()
    }

    /// Exposed for raw by-offset access methods (use with `open_dir_at_end`).
    #[inline]
    pub fn entry_range_info(&self) -> &accessor::EntryRangeInfo {
        self.inner.entry_range_info()
    }
}

impl<T: Clone + ReadAt> std::ops::Deref for FileEntry<T> {
    type Target = Entry;

    fn deref(&self) -> &Self::Target {
        self.entry()
    }
}

/// An iterator over the contents of a `Directory`.
#[repr(transparent)]
pub struct ReadDir<'a, T> {
    inner: accessor::ReadDirImpl<'a, T>,
}

impl<'a, T: Clone + ReadAt> ReadDir<'a, T> {
    /// Efficient alternative to `Iterator::skip`.
    #[inline]
    pub fn skip(self, n: usize) -> Self {
        Self {
            inner: self.inner.skip(n),
        }
    }

    /// Efficient alternative to `Iterator::count`.
    #[inline]
    pub fn count(self) -> usize {
        self.inner.count()
    }

    /// Get the next directory entry.
    pub async fn next(&mut self) -> Option<io::Result<DirEntry<'a, T>>> {
        match self.inner.next().await {
            Ok(Some(inner)) => Some(Ok(DirEntry { inner })),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        }
    }
}

/// A directory entry. When iterating through the contents of a directory we first get access to
/// the file name. The remaining information can be decoded afterwards.
#[repr(transparent)]
pub struct DirEntry<'a, T: Clone + ReadAt> {
    inner: accessor::DirEntryImpl<'a, T>,
}

impl<'a, T: Clone + ReadAt> DirEntry<'a, T> {
    /// Get the current file name.
    pub fn file_name(&self) -> &Path {
        self.inner.file_name()
    }

    /// Decode the entry.
    ///
    /// When iterating over a directory, the file name is read separately from the file attributes,
    /// so only the file name is available here, while the attributes still need to be decoded.
    pub async fn decode_entry(&self) -> io::Result<FileEntry<T>> {
        self.inner
            .decode_entry()
            .await
            .map(|inner| FileEntry { inner })
    }

    /// Exposed for raw by-offset access methods.
    #[inline]
    pub fn entry_range_info(&self) -> &accessor::EntryRangeInfo {
        self.inner.entry_range_info()
    }
}

/// File content read future result.
struct ReadResult {
    len: usize,
    buffer: Vec<u8>,
}

/// A reader for file contents.
pub struct FileContents<T> {
    inner: accessor::FileContentsImpl<T>,
    at: u64,
    buffer: Vec<u8>,
    future: Option<Pin<Box<dyn Future<Output = io::Result<ReadResult>> + 'static>>>,
}

// We lose `Send` via the boxed trait object and don't want to force the trait object to
// potentially be more strict than `T`, so we leave it as it is and implement Send and Sync
// depending on T.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T: Send> Send for FileContents<T> {}
unsafe impl<T: Sync> Sync for FileContents<T> {}

#[cfg(feature = "tokio-io")]
impl<T: Clone + ReadAt> tokio::io::AsyncRead for FileContents<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        dest: &mut tokio::io::ReadBuf,
    ) -> Poll<io::Result<()>> {
        let this = unsafe { Pin::into_inner_unchecked(self) };
        loop {
            match this.future.take() {
                None => {
                    let mut buffer = mem::take(&mut this.buffer);
                    util::scale_read_buffer(&mut buffer, dest.remaining());
                    let reader: accessor::FileContentsImpl<T> = this.inner.clone();
                    let at = this.at;
                    let future: Pin<Box<dyn Future<Output = io::Result<ReadResult>>>> =
                        Box::pin(async move {
                            let len = reader.read_at(&mut buffer, at).await?;
                            io::Result::Ok(ReadResult { len, buffer })
                        });
                    // This future has the lifetime from T. Self also has this lifetime and we
                    // store this in a pinned self. T maybe a reference with a non-'static life
                    // time, but then it cannot be a self-reference into Self, so this should be
                    // valid in all cases:
                    this.future = Some(unsafe { mem::transmute(future) });
                }
                Some(mut fut) => match fut.as_mut().poll(cx) {
                    Poll::Pending => {
                        this.future = Some(fut);
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
                    Poll::Ready(Ok(ReadResult { len: got, buffer })) => {
                        this.buffer = buffer;
                        this.at += got as u64;
                        let len = got.min(dest.remaining());
                        dest.put_slice(&this.buffer[..len]);
                        return Poll::Ready(Ok(()));
                    }
                },
            }
        }
    }
}

impl<T: Clone + ReadAt> ReadAt for FileContents<T> {
    fn start_read_at<'a>(
        self: Pin<&'a Self>,
        cx: &mut Context,
        buf: &'a mut [u8],
        offset: u64,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>> {
        unsafe { self.map_unchecked(|this| &this.inner) }.start_read_at(cx, buf, offset)
    }

    fn poll_complete<'a>(
        self: Pin<&'a Self>,
        op: ReadAtOperation<'a>,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>> {
        unsafe { self.map_unchecked(|this| &this.inner) }.poll_complete(op)
    }
}

impl<T: Clone + ReadAt> FileContents<T> {
    /// Convenience helper for `read_at`:
    pub async fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        self.inner.read_at(buf, offset).await
    }
}
