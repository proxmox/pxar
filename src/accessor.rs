//! Random access for PXAR files.

use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::{self, size_of, size_of_val, MaybeUninit};
use std::ops::Range;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use endian_trait::Endian;

use crate::binary_tree_array;
use crate::decoder::{self, DecoderImpl};
use crate::format::{self, GoodbyeItem};
use crate::poll_fn::poll_fn;
use crate::util;
use crate::{Entry, EntryKind};

#[doc(hidden)]
pub mod aio;
pub mod cache;
pub mod sync;

#[doc(inline)]
pub use sync::{Accessor, DirEntry, Directory, FileEntry, ReadDir};

use cache::Cache;

/// Random access read implementation.
pub trait ReadAt {
    fn poll_read_at(
        self: Pin<&Self>,
        cx: &mut Context,
        buf: &mut [u8],
        offset: u64,
    ) -> Poll<io::Result<usize>>;
}

/// We do not want to bother with actual polling, so we implement `async fn` variants of the above
/// on `dyn ReadAt`.
///
/// The reason why this is not an internal `ReadAtExt` trait like `AsyncReadExt` is simply that
/// we'd then need to define all the `Future` types they return manually and explicitly. Since we
/// have no use for them, all we want is the ability to use `async fn`...
///
/// The downside is that we need some `(&mut self.input as &mut dyn ReadAt)` casts in the
/// decoder's code, but that's fine.
impl<'a> dyn ReadAt + 'a {
    /// awaitable version of `poll_read_at`.
    async fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        poll_fn(|cx| unsafe { Pin::new_unchecked(self).poll_read_at(cx, buf, offset) }).await
    }

    /// `read_exact_at` - since that's what we _actually_ want most of the time.
    async fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.read_at(buf, offset).await? {
                0 => io_bail!("unexpected EOF"),
                got => {
                    buf = &mut buf[got..];
                    offset += got as u64;
                }
            }
        }
        Ok(())
    }

    /// Helper to read into an `Endian`-implementing `struct`.
    async fn read_entry_at<T: Endian>(&self, offset: u64) -> io::Result<T> {
        let mut data = MaybeUninit::<T>::uninit();
        let buf =
            unsafe { std::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut u8, size_of::<T>()) };
        self.read_exact_at(buf, offset).await?;
        Ok(unsafe { data.assume_init().from_le() })
    }

    /// Helper to read into an allocated byte vector.
    async fn read_exact_data_at(&self, size: usize, offset: u64) -> io::Result<Vec<u8>> {
        let mut data = util::vec_new(size);
        self.read_exact_at(&mut data[..], offset).await?;
        Ok(data)
    }
}

/// Allow using trait objects for `T: ReadAt`
impl<'a> ReadAt for &(dyn ReadAt + 'a) {
    fn poll_read_at(
        self: Pin<&Self>,
        cx: &mut Context,
        buf: &mut [u8],
        offset: u64,
    ) -> Poll<io::Result<usize>> {
        unsafe {
            Pin::new_unchecked(&**self).poll_read_at(cx, buf, offset)
        }
    }
}

#[derive(Clone)]
struct Caches {
    /// The goodbye table cache maps goodbye table offsets to cache entries.
    gbt_cache: Option<Arc<dyn Cache<u64, [GoodbyeItem]> + Send + Sync>>,
}

impl Default for Caches {
    fn default() -> Self {
        Self { gbt_cache: None }
    }
}

/// The random access state machine implementation.
pub(crate) struct AccessorImpl<T> {
    input: T,
    size: u64,
    caches: Arc<Caches>,
}

impl<T: ReadAt> AccessorImpl<T> {
    pub async fn new(input: T, size: u64) -> io::Result<Self> {
        if size < (size_of::<GoodbyeItem>() as u64) {
            io_bail!("too small to contain a pxar archive");
        }

        Ok(Self {
            input,
            size,
            caches: Arc::new(Caches::default()),
        })
    }

    pub async fn open_root_ref<'a>(&'a self) -> io::Result<DirectoryImpl<&'a dyn ReadAt>> {
        DirectoryImpl::open_at_end(
            &self.input as &dyn ReadAt,
            self.size,
            "/".into(),
            Arc::clone(&self.caches),
        )
        .await
    }

    pub fn set_goodbye_table_cache(
        &mut self,
        cache: Option<Arc<dyn Cache<u64, [GoodbyeItem]> + Send + Sync>>,
    ) {
        let new_caches = Arc::new(Caches {
            gbt_cache: cache,
            ..*self.caches
        });
        self.caches = new_caches;
    }
}

impl<T: Clone + ReadAt> AccessorImpl<T> {
    pub async fn open_root(&self) -> io::Result<DirectoryImpl<T>> {
        DirectoryImpl::open_at_end(
            self.input.clone(),
            self.size,
            "/".into(),
            Arc::clone(&self.caches),
        )
        .await
    }
}

/// The directory random-access state machine implementation.
pub(crate) struct DirectoryImpl<T> {
    input: T,
    entry_ofs: u64,
    goodbye_ofs: u64,
    size: u64,
    table: Arc<[GoodbyeItem]>,
    path: PathBuf,
    caches: Arc<Caches>,
}

impl<T: Clone + ReadAt> DirectoryImpl<T> {
    /// Open a directory ending at the specified position.
    async fn open_at_end(
        input: T,
        end_offset: u64,
        path: PathBuf,
        caches: Arc<Caches>,
    ) -> io::Result<DirectoryImpl<T>> {
        let tail = Self::read_tail_entry(&input, end_offset).await?;

        if end_offset < tail.size {
            io_bail!("goodbye tail size out of range");
        }

        let goodbye_ofs = end_offset - tail.size;

        if goodbye_ofs < tail.offset {
            io_bail!("goodbye offset out of range");
        }

        let entry_ofs = goodbye_ofs - tail.offset;
        let size = end_offset - entry_ofs;

        let table: Option<Arc<[GoodbyeItem]>> = caches
            .gbt_cache
            .as_ref()
            .and_then(|cache| cache.fetch(goodbye_ofs));

        let mut this = Self {
            input,
            entry_ofs,
            goodbye_ofs,
            size,
            table: table.as_ref().map_or_else(|| Arc::new([]), Arc::clone),
            path,
            caches,
        };

        // sanity check:
        if this.table_size() % (size_of::<GoodbyeItem>() as u64) != 0 {
            io_bail!("invalid goodbye table size: {}", this.table_size());
        }

        if table.is_none() {
            this.table = this.load_table().await?;
            if let Some(ref cache) = this.caches.gbt_cache {
                cache.insert(goodbye_ofs, Arc::clone(&this.table));
            }
        }

        Ok(this)
    }

    /// Load the entire goodbye table:
    async fn load_table(&self) -> io::Result<Arc<[GoodbyeItem]>> {
        let len = self.len();
        let mut data = Vec::with_capacity(self.len());
        unsafe {
            data.set_len(len);
            let slice = std::slice::from_raw_parts_mut(
                data.as_mut_ptr() as *mut u8,
                len * size_of::<GoodbyeItem>(),
            );
            (&self.input as &dyn ReadAt)
                .read_exact_at(slice, self.table_offset())
                .await?;
            drop(slice);
        }
        Ok(Arc::from(data))
    }

    #[inline]
    fn end_offset(&self) -> u64 {
        self.entry_ofs + self.size
    }

    #[inline]
    fn entry_range(&self) -> Range<u64> {
        self.entry_ofs..self.end_offset()
    }

    #[inline]
    fn table_size(&self) -> u64 {
        (self.end_offset() - self.goodbye_ofs) - (size_of::<format::Header>() as u64)
    }

    #[inline]
    fn table_offset(&self) -> u64 {
        self.goodbye_ofs + (size_of::<format::Header>() as u64)
    }

    /// Length *excluding* the tail marker!
    #[inline]
    fn len(&self) -> usize {
        (self.table_size() / (size_of::<GoodbyeItem>() as u64)) as usize - 1
    }

    /// Read the goodbye tail and perform some sanity checks.
    async fn read_tail_entry(input: &'_ dyn ReadAt, end_offset: u64) -> io::Result<GoodbyeItem> {
        if end_offset < (size_of::<GoodbyeItem>() as u64) {
            io_bail!("goodbye tail does not fit");
        }

        let tail_offset = end_offset - (size_of::<GoodbyeItem>() as u64);
        let tail: GoodbyeItem = input.read_entry_at(tail_offset).await?;

        if tail.hash != format::PXAR_GOODBYE_TAIL_MARKER {
            io_bail!("no goodbye tail marker found");
        }

        Ok(tail)
    }

    /// Get a decoder for the directory contents.
    pub(crate) async fn decode_full(&self) -> io::Result<DecoderImpl<SeqReadAtAdapter<T>>> {
        let (dir, decoder) = self.decode_one_entry(self.entry_range(), None).await?;
        if !dir.is_dir() {
            io_bail!("directory does not seem to be a directory");
        }
        Ok(decoder)
    }

    async fn get_decoder(
        &self,
        entry_range: Range<u64>,
        file_name: Option<&Path>,
    ) -> io::Result<DecoderImpl<SeqReadAtAdapter<T>>> {
        Ok(DecoderImpl::new_full(
            SeqReadAtAdapter::new(self.input.clone(), entry_range),
            match file_name {
                None => self.path.clone(),
                Some(file) => self.path.join(file),
            },
        )
        .await?)
    }

    async fn decode_one_entry(
        &self,
        entry_range: Range<u64>,
        file_name: Option<&Path>,
    ) -> io::Result<(Entry, DecoderImpl<SeqReadAtAdapter<T>>)> {
        let mut decoder = self.get_decoder(entry_range, file_name).await?;
        let entry = decoder
            .next()
            .await
            .ok_or_else(|| io_format_err!("unexpected EOF while decoding directory entry"))??;
        Ok((entry, decoder))
    }

    fn lookup_hash_position(&self, hash: u64, start: usize, skip: usize) -> Option<usize> {
        binary_tree_array::search_by(&self.table, start, skip, |i| hash.cmp(&i.hash))
    }

    async fn lookup_self(&self) -> io::Result<FileEntryImpl<T>> {
        let (entry, _decoder) = self.decode_one_entry(self.entry_range(), None).await?;
        Ok(FileEntryImpl {
            input: self.input.clone(),
            entry,
            end_offset: self.end_offset(),
            caches: Arc::clone(&self.caches),
        })
    }

    /// Lookup a directory entry.
    pub async fn lookup(&self, path: &Path) -> io::Result<Option<FileEntryImpl<T>>> {
        let mut cur: Option<FileEntryImpl<T>> = None;

        let mut first = true;
        for component in path.components() {
            use std::path::Component;

            let first = mem::replace(&mut first, false);

            let component = match component {
                Component::Normal(path) => path,
                Component::ParentDir => io_bail!("cannot enter parent directory in archive"),
                Component::RootDir | Component::CurDir if first => {
                    cur = Some(self.lookup_self().await?);
                    continue;
                }
                Component::CurDir => continue,
                _ => io_bail!("invalid component in path"),
            };

            let next = match cur {
                Some(entry) => {
                    entry
                        .enter_directory()
                        .await?
                        .lookup_component(component)
                        .await?
                }
                None => self.lookup_component(component).await?,
            };

            if next.is_none() {
                return Ok(None);
            }

            cur = next;
        }

        Ok(cur)
    }

    /// Lookup a single directory entry component (does not handle multiple components in path)
    pub async fn lookup_component(&self, path: &OsStr) -> io::Result<Option<FileEntryImpl<T>>> {
        let hash = format::hash_filename(path.as_bytes());
        let first_index = match self.lookup_hash_position(hash, 0, 0) {
            Some(index) => index,
            None => return Ok(None),
        };

        // Lookup FILENAME, if the hash matches but the filename doesn't, check for a duplicate
        // hash once found, use the GoodbyeItem's offset+size as well as the file's Entry to return
        // a DirEntry::Dir or Dir::Entry.
        //
        let mut dup = 0;
        loop {
            let index = match self.lookup_hash_position(hash, first_index, dup) {
                Some(index) => index,
                None => return Ok(None),
            };

            let cursor = self.get_cursor(index).await?;
            if cursor.file_name == path {
                return Ok(Some(cursor.decode_entry().await?));
            }

            dup += 1;
        }
    }

    async fn get_cursor<'a>(&'a self, index: usize) -> io::Result<DirEntryImpl<'a, T>> {
        let entry = &self.table[index];
        let file_goodbye_ofs = entry.offset;
        if self.goodbye_ofs < file_goodbye_ofs {
            io_bail!("invalid file offset");
        }

        let file_ofs = self.goodbye_ofs - file_goodbye_ofs;
        let (file_name, entry_ofs) = self.read_filename_entry(file_ofs).await?;

        let entry_range = Range {
            start: entry_ofs,
            end: file_ofs + entry.size,
        };
        if entry_range.end < entry_range.start {
            io_bail!(
                "bad file: invalid entry ranges for {:?}: \
                 start=0x{:x}, file_ofs=0x{:x}, size=0x{:x}",
                file_name,
                entry_ofs,
                file_ofs,
                entry.size,
            );
        }

        Ok(DirEntryImpl {
            dir: self,
            file_name,
            entry_range,
            caches: Arc::clone(&self.caches),
        })
    }

    async fn read_filename_entry(&self, file_ofs: u64) -> io::Result<(PathBuf, u64)> {
        let head: format::Header = (&self.input as &dyn ReadAt).read_entry_at(file_ofs).await?;
        if head.htype != format::PXAR_FILENAME {
            io_bail!("expected PXAR_FILENAME header, found: {:x}", head.htype);
        }

        let mut path = (&self.input as &dyn ReadAt)
            .read_exact_data_at(
                head.content_size() as usize,
                file_ofs + (size_of_val(&head) as u64),
            )
            .await?;

        if path.pop() != Some(0) {
            io_bail!("invalid file name (missing terminating zero)");
        }

        if path.is_empty() {
            io_bail!("invalid empty file name");
        }

        let file_name = PathBuf::from(OsString::from_vec(path));
        format::check_file_name(&file_name)?;

        Ok((file_name, file_ofs + head.full_size()))
    }

    pub fn read_dir(&self) -> ReadDirImpl<T> {
        ReadDirImpl::new(self, 0)
    }
}

/// A file entry retrieved from a Directory.
pub(crate) struct FileEntryImpl<T: Clone + ReadAt> {
    input: T,
    entry: Entry,
    end_offset: u64,
    caches: Arc<Caches>,
}

impl<T: Clone + ReadAt> FileEntryImpl<T> {
    pub async fn enter_directory(&self) -> io::Result<DirectoryImpl<T>> {
        if !self.entry.is_dir() {
            io_bail!("enter_directory() on a non-directory");
        }

        DirectoryImpl::open_at_end(
            self.input.clone(),
            self.end_offset,
            self.entry.path.clone(),
            Arc::clone(&self.caches),
        )
        .await
    }

    pub async fn contents(&self) -> io::Result<FileContentsImpl<T>> {
        match self.entry.kind {
            EntryKind::File { offset: None, .. } => {
                io_bail!("cannot open file, reader provided no offset")
            }
            EntryKind::File {
                size,
                offset: Some(offset),
            } => Ok(FileContentsImpl::new(
                self.input.clone(),
                offset..(offset + size),
            )),
            _ => io_bail!("not a file"),
        }
    }

    #[inline]
    pub fn into_entry(self) -> Entry {
        self.entry
    }

    #[inline]
    pub fn entry(&self) -> &Entry {
        &self.entry
    }
}

/// An iterator over the contents of a directory.
pub(crate) struct ReadDirImpl<'a, T> {
    dir: &'a DirectoryImpl<T>,
    at: usize,
}

impl<'a, T: Clone + ReadAt> ReadDirImpl<'a, T> {
    fn new(dir: &'a DirectoryImpl<T>, at: usize) -> Self {
        Self { dir, at }
    }

    /// Get the next entry.
    pub async fn next(&mut self) -> io::Result<Option<DirEntryImpl<'a, T>>> {
        if self.at == self.dir.table.len() {
            Ok(None)
        } else {
            let cursor = self.dir.get_cursor(self.at).await?;
            self.at += 1;
            Ok(Some(cursor))
        }
    }

    /// Efficient alternative to `Iterator::skip`.
    #[inline]
    pub fn skip(self, n: usize) -> Self {
        Self {
            at: (self.at + n).min(self.dir.table.len()),
            dir: self.dir,
        }
    }

    /// Efficient alternative to `Iterator::count`.
    #[inline]
    pub fn count(self) -> usize {
        self.dir.table.len()
    }
}

/// A cursor pointing to a file in a directory.
///
/// At this point only the file name has been read and we remembered the position for finding the
/// actual data. This can be upgraded into a FileEntryImpl.
pub(crate) struct DirEntryImpl<'a, T: Clone + ReadAt> {
    dir: &'a DirectoryImpl<T>,
    file_name: PathBuf,
    entry_range: Range<u64>,
    caches: Arc<Caches>,
}

impl<'a, T: Clone + ReadAt> DirEntryImpl<'a, T> {
    pub fn file_name(&self) -> &Path {
        &self.file_name
    }

    async fn decode_entry(&self) -> io::Result<FileEntryImpl<T>> {
        let end_offset = self.entry_range.end;
        let (entry, _decoder) = self
            .dir
            .decode_one_entry(self.entry_range.clone(), Some(&self.file_name))
            .await?;

        Ok(FileEntryImpl {
            input: self.dir.input.clone(),
            entry,
            end_offset,
            caches: Arc::clone(&self.caches),
        })
    }
}

/// A reader for file contents.
pub(crate) struct FileContentsImpl<T> {
    input: T,

    /// Absolute offset inside the `input`.
    range: Range<u64>,
}

impl<T: Clone + ReadAt> FileContentsImpl<T> {
    pub fn new(input: T, range: Range<u64>) -> Self {
        Self { input, range }
    }

    #[inline]
    pub fn file_size(&self) -> u64 {
        self.range.end - self.range.start
    }

    async fn read_at(&self, mut buf: &mut [u8], offset: u64) -> io::Result<usize> {
        let size = self.file_size();
        if offset >= size {
            return Ok(0);
        }
        let remaining = size - offset;

        if remaining < buf.len() as u64 {
            buf = &mut buf[..(remaining as usize)];
        }

        (&self.input as &dyn ReadAt)
            .read_at(buf, self.range.start + offset)
            .await
    }
}

#[doc(hidden)]
pub struct SeqReadAtAdapter<T> {
    input: T,
    range: Range<u64>,
}

impl<T: ReadAt> SeqReadAtAdapter<T> {
    pub fn new(input: T, range: Range<u64>) -> Self {
        if range.end < range.start {
            panic!("BAD SEQ READ AT ADAPTER");
        }
        Self { input, range }
    }

    #[inline]
    fn remaining(&self) -> usize {
        (self.range.end - self.range.start) as usize
    }
}

impl<T: ReadAt> decoder::SeqRead for SeqReadAtAdapter<T> {
    fn poll_seq_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let len = buf.len().min(self.remaining());
        let buf = &mut buf[..len];

        let this = unsafe { self.get_unchecked_mut() };

        let got = ready!(unsafe {
            Pin::new_unchecked(&this.input).poll_read_at(cx, buf, this.range.start)
        })?;
        this.range.start += got as u64;
        Poll::Ready(Ok(got))
    }

    fn poll_position(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Option<io::Result<u64>>> {
        Poll::Ready(Some(Ok(self.range.start)))
    }
}
