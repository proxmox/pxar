//! Random access for PXAR files.

#![deny(missing_docs)]

use std::ffi::{OsStr, OsString};
use std::future::Future;
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
use crate::util;
use crate::{Entry, EntryKind};

pub mod aio;
pub mod cache;
pub mod sync;

pub mod read_at;

#[doc(inline)]
pub use sync::{Accessor, DirEntry, Directory, FileEntry, ReadDir};

#[doc(inline)]
pub use read_at::{MaybeReady, ReadAt, ReadAtExt, ReadAtOperation};

use cache::Cache;

/// Range information used for unsafe raw random access:
#[derive(Clone, Debug)]
pub struct EntryRangeInfo {
    /// Offset to the `FILENAME` header.
    pub filename_header_offset: Option<u64>,
    /// Byte range spanning an entry in a pxar archive.
    pub entry_range: Range<u64>,
}

impl EntryRangeInfo {
    /// Shortcut to create the "toplevel" range info without file name header offset.
    pub fn toplevel(entry_range: Range<u64>) -> Self {
        Self {
            filename_header_offset: None,
            entry_range,
        }
    }
}

/// awaitable version of `ReadAt`.
async fn read_at<T>(input: &T, buf: &mut [u8], offset: u64) -> io::Result<usize>
where
    T: ReadAtExt,
{
    input.read_at(buf, offset).await
}

/// `read_exact_at` - since that's what we _actually_ want most of the time.
async fn read_exact_at<T>(input: &T, mut buf: &mut [u8], mut offset: u64) -> io::Result<()>
where
    T: ReadAt,
{
    while !buf.is_empty() {
        match read_at(input, buf, offset).await? {
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
async fn read_entry_at<T, E: Endian>(input: &T, offset: u64) -> io::Result<E>
where
    T: ReadAt,
{
    let mut data = MaybeUninit::<E>::uninit();
    let buf =
        unsafe { std::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut u8, size_of::<E>()) };
    read_exact_at(input, buf, offset).await?;
    Ok(unsafe { data.assume_init().from_le() })
}

/// Helper to read into an allocated byte vector.
async fn read_exact_data_at<T>(input: &T, size: usize, offset: u64) -> io::Result<Vec<u8>>
where
    T: ReadAt,
{
    let mut data = unsafe { util::vec_new_uninitialized(size) };
    read_exact_at(input, &mut data[..], offset).await?;
    Ok(data)
}

/// Allow using trait objects for `T: ReadAt`
impl<'d> ReadAt for &(dyn ReadAt + 'd) {
    fn start_read_at<'a>(
        self: Pin<&'a Self>,
        cx: &mut Context,
        buf: &'a mut [u8],
        offset: u64,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>> {
        unsafe { Pin::new_unchecked(&**self).start_read_at(cx, buf, offset) }
    }

    fn poll_complete<'a>(
        self: Pin<&'a Self>,
        op: ReadAtOperation<'a>,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>> {
        unsafe { Pin::new_unchecked(&**self).poll_complete(op) }
    }
}

/// Convenience impl for `Arc<dyn ReadAt + Send + Sync + 'static>`. Since `ReadAt` only requires
/// immutable `&self`, this adds some convenience by allowing to just `Arc` any `'static` type that
/// implemments `ReadAt` for type monomorphization.
impl ReadAt for Arc<dyn ReadAt + Send + Sync + 'static> {
    fn start_read_at<'a>(
        self: Pin<&'a Self>,
        cx: &mut Context,
        buf: &'a mut [u8],
        offset: u64,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>> {
        unsafe {
            self.map_unchecked(|this| &**this)
                .start_read_at(cx, buf, offset)
        }
    }

    fn poll_complete<'a>(
        self: Pin<&'a Self>,
        op: ReadAtOperation<'a>,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>> {
        unsafe { self.map_unchecked(|this| &**this).poll_complete(op) }
    }
}

/// Convenience impl for in-memory byte slices.
impl ReadAt for &'_ [u8] {
    fn start_read_at<'a>(
        self: Pin<&'a Self>,
        _cx: &mut Context,
        buf: &'a mut [u8],
        offset: u64,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>> {
        if offset >= self.len() as u64 {
            return MaybeReady::Ready(Ok(0));
        }

        let offset = offset as usize;
        let end = (offset + buf.len()).min(self.len());
        let size = end - offset;
        buf[..size].copy_from_slice(&self[offset..end]);
        MaybeReady::Ready(Ok(size))
    }

    fn poll_complete<'a>(
        self: Pin<&'a Self>,
        _op: ReadAtOperation<'a>,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>> {
        panic!("start_read_at on byte slice returned Pending");
    }
}

#[derive(Clone, Default)]
struct Caches {
    /// The goodbye table cache maps goodbye table offsets to cache entries.
    gbt_cache: Option<Arc<dyn Cache<u64, [GoodbyeItem]> + Send + Sync>>,
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

        let header: format::Header = read_entry_at(&input, 0).await?;
        header.check_header_size()?;

        if header.htype == format::PXAR_FORMAT_VERSION {
            let version: u64 = read_entry_at(
                &input,
                size_of::<format::Header>() as u64,
            )
            .await?;
            io_bail!("format version {version} not compatible with this client");
        }

        Ok(Self {
            input,
            size,
            caches: Arc::new(Caches::default()),
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub async fn open_root_ref(&self) -> io::Result<DirectoryImpl<&dyn ReadAt>> {
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
            //..*self.caches
        });
        self.caches = new_caches;
    }
}

async fn get_decoder<T: ReadAt>(
    input: T,
    entry_range: Range<u64>,
    path: PathBuf,
) -> io::Result<DecoderImpl<SeqReadAtAdapter<T>>> {
    DecoderImpl::new_full(SeqReadAtAdapter::new(input, entry_range), path, true).await
}

// NOTE: This performs the Decoder::read_next_item() behavior! Keep in mind when changing!
async fn get_decoder_at_filename<T: ReadAt>(
    input: T,
    entry_range: Range<u64>,
    path: PathBuf,
) -> io::Result<(DecoderImpl<SeqReadAtAdapter<T>>, u64)> {
    // Read the header, it should be a FILENAME, then skip over it and its length:
    let header: format::Header = read_entry_at(&input, entry_range.start).await?;
    header.check_header_size()?;

    if header.htype != format::PXAR_FILENAME {
        io_bail!("expected filename entry, got {:?}", header);
    }

    let entry_offset = entry_range.start + header.full_size();
    if entry_offset >= entry_range.end {
        io_bail!("filename exceeds current file range");
    }

    Ok((
        get_decoder(input, entry_offset..entry_range.end, path).await?,
        entry_offset,
    ))
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

    /// Allow opening a directory at a specified offset.
    pub async unsafe fn open_dir_at_end(&self, offset: u64) -> io::Result<DirectoryImpl<T>> {
        DirectoryImpl::open_at_end(
            self.input.clone(),
            offset,
            "/".into(),
            Arc::clone(&self.caches),
        )
        .await
    }

    /// Allow opening a regular file from a specified range.
    pub async unsafe fn open_file_at_range(
        &self,
        entry_range_info: &EntryRangeInfo,
    ) -> io::Result<FileEntryImpl<T>> {
        let mut decoder = get_decoder(
            self.input.clone(),
            entry_range_info.entry_range.clone(),
            PathBuf::new(),
        )
        .await?;
        let entry = decoder
            .next()
            .await
            .ok_or_else(|| io_format_err!("unexpected EOF while decoding file entry"))??;
        Ok(FileEntryImpl {
            input: self.input.clone(),
            entry,
            entry_range_info: entry_range_info.clone(),
            caches: Arc::clone(&self.caches),
        })
    }

    /// Allow opening arbitrary contents from a specific range.
    pub unsafe fn open_contents_at_range(&self, range: Range<u64>) -> FileContentsImpl<T> {
        FileContentsImpl::new(self.input.clone(), range)
    }

    /// Following a hardlink breaks a couple of conventions we otherwise have, particularly we will
    /// never know the actual length of the target entry until we're done decoding it, so this
    /// needs to happen at the accessor level, rather than a "sub-entry-reader".
    pub async fn follow_hardlink(&self, entry: &FileEntryImpl<T>) -> io::Result<FileEntryImpl<T>> {
        let link_offset = match entry.entry.kind() {
            EntryKind::Hardlink(link) => link.offset,
            _ => io_bail!("cannot resolve a non-hardlink"),
        };

        let entry_file_offset = entry
            .entry_range_info
            .filename_header_offset
            .ok_or_else(|| io_format_err!("cannot follow hardlink without a file entry header"))?;

        if link_offset > entry_file_offset {
            io_bail!("invalid offset in hardlink");
        }

        let link_offset = entry_file_offset - link_offset;

        let (mut decoder, entry_offset) =
            get_decoder_at_filename(self.input.clone(), link_offset..self.size, PathBuf::new())
                .await?;

        let entry = decoder
            .next()
            .await
            .ok_or_else(|| io_format_err!("unexpected EOF while following a hardlink"))??;

        match entry.kind() {
            EntryKind::File { offset: None, .. } => {
                io_bail!("failed to follow hardlink, reader provided no offsets");
            }
            EntryKind::File {
                offset: Some(offset),
                size,
            } => {
                let meta_size = offset - link_offset;
                let entry_end = link_offset + meta_size + size;
                Ok(FileEntryImpl {
                    input: self.input.clone(),
                    entry,
                    entry_range_info: EntryRangeInfo {
                        filename_header_offset: Some(link_offset),
                        entry_range: entry_offset..entry_end,
                    },
                    caches: Arc::clone(&self.caches),
                })
            }
            _ => io_bail!("hardlink does not point to a regular file"),
        }
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
        let mut data;
        unsafe {
            data = crate::util::vec_new_uninitialized(self.len());
            let slice = std::slice::from_raw_parts_mut(
                data.as_mut_ptr() as *mut u8,
                len * size_of::<GoodbyeItem>(),
            );
            read_exact_at(&self.input, slice, self.table_offset()).await?;
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
    async fn read_tail_entry(input: &T, end_offset: u64) -> io::Result<GoodbyeItem> {
        if end_offset < (size_of::<GoodbyeItem>() as u64) {
            io_bail!("goodbye tail does not fit");
        }

        let tail_offset = end_offset - (size_of::<GoodbyeItem>() as u64);
        let tail: GoodbyeItem = read_entry_at(input, tail_offset).await?;

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
        get_decoder(
            self.input.clone(),
            entry_range,
            match file_name {
                None => self.path.clone(),
                Some(file) => self.path.join(file),
            },
        )
        .await
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

    pub async fn lookup_self(&self) -> io::Result<FileEntryImpl<T>> {
        let (entry, _decoder) = self.decode_one_entry(self.entry_range(), None).await?;
        Ok(FileEntryImpl {
            input: self.input.clone(),
            entry,
            entry_range_info: EntryRangeInfo {
                filename_header_offset: None,
                entry_range: self.entry_range(),
            },
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

    // while clippy is technically right about this, the compiler won't accept it (yet)
    #[allow(clippy::needless_lifetimes)]
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
            entry_range_info: EntryRangeInfo {
                filename_header_offset: Some(file_ofs),
                entry_range,
            },
            caches: Arc::clone(&self.caches),
        })
    }

    async fn read_filename_entry(&self, file_ofs: u64) -> io::Result<(PathBuf, u64)> {
        let head: format::Header = read_entry_at(&self.input, file_ofs).await?;
        if head.htype != format::PXAR_FILENAME {
            io_bail!("expected PXAR_FILENAME header, found: {}", head);
        }

        let mut path = read_exact_data_at(
            &self.input,
            head.content_size() as usize,
            file_ofs + (size_of_val(&head) as u64),
        )
        .await?;

        if path.pop() != Some(0) {
            io_bail!("invalid file name (missing terminating zero)");
        }

        crate::util::validate_filename(&path)?;

        let file_name = PathBuf::from(OsString::from_vec(path));
        format::check_file_name(&file_name)?;

        Ok((file_name, file_ofs + head.full_size()))
    }

    pub fn read_dir(&self) -> ReadDirImpl<T> {
        ReadDirImpl::new(self, 0)
    }

    pub fn entry_count(&self) -> usize {
        self.table.len()
    }
}

/// A file entry retrieved from a Directory.
#[derive(Clone)]
pub(crate) struct FileEntryImpl<T: Clone + ReadAt> {
    input: T,
    entry: Entry,
    entry_range_info: EntryRangeInfo,
    caches: Arc<Caches>,
}

impl<T: Clone + ReadAt> FileEntryImpl<T> {
    pub async fn enter_directory(&self) -> io::Result<DirectoryImpl<T>> {
        if !self.entry.is_dir() {
            io_bail!("enter_directory() on a non-directory");
        }

        DirectoryImpl::open_at_end(
            self.input.clone(),
            self.entry_range_info.entry_range.end,
            self.entry.path.clone(),
            Arc::clone(&self.caches),
        )
        .await
    }

    /// For use with unsafe accessor methods.
    pub fn content_range(&self) -> io::Result<Option<Range<u64>>> {
        match self.entry.kind {
            EntryKind::File { offset: None, .. } => {
                io_bail!("cannot open file, reader provided no offset")
            }
            EntryKind::File {
                size,
                offset: Some(offset),
            } => Ok(Some(offset..(offset + size))),
            _ => Ok(None),
        }
    }

    pub async fn contents(&self) -> io::Result<FileContentsImpl<T>> {
        match self.content_range()? {
            Some(range) => Ok(FileContentsImpl::new(self.input.clone(), range)),
            None => io_bail!("not a file"),
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

    /// Exposed for raw by-offset access methods (use with `open_dir_at_end`).
    #[inline]
    pub fn entry_range_info(&self) -> &EntryRangeInfo {
        &self.entry_range_info
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
    entry_range_info: EntryRangeInfo,
    caches: Arc<Caches>,
}

impl<'a, T: Clone + ReadAt> DirEntryImpl<'a, T> {
    pub fn file_name(&self) -> &Path {
        &self.file_name
    }

    async fn decode_entry(&self) -> io::Result<FileEntryImpl<T>> {
        let (entry, _decoder) = self
            .dir
            .decode_one_entry(
                self.entry_range_info.entry_range.clone(),
                Some(&self.file_name),
            )
            .await?;

        Ok(FileEntryImpl {
            input: self.dir.input.clone(),
            entry,
            entry_range_info: self.entry_range_info.clone(),
            caches: Arc::clone(&self.caches),
        })
    }

    /// Exposed for raw by-offset access methods.
    #[inline]
    pub fn entry_range_info(&self) -> &EntryRangeInfo {
        &self.entry_range_info
    }
}

/// A reader for file contents.
#[derive(Clone)]
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

        read_at(&self.input, buf, self.range.start + offset).await
    }
}

impl<T: Clone + ReadAt> ReadAt for FileContentsImpl<T> {
    fn start_read_at<'a>(
        self: Pin<&'a Self>,
        cx: &mut Context,
        mut buf: &'a mut [u8],
        offset: u64,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>> {
        let size = self.file_size();
        if offset >= size {
            return MaybeReady::Ready(Ok(0));
        }
        let remaining = size - offset;

        if remaining < buf.len() as u64 {
            buf = &mut buf[..(remaining as usize)];
        }

        let offset = self.range.start + offset;
        unsafe { self.map_unchecked(|this| &this.input) }.start_read_at(cx, buf, offset)
    }

    fn poll_complete<'a>(
        self: Pin<&'a Self>,
        op: ReadAtOperation<'a>,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>> {
        unsafe { self.map_unchecked(|this| &this.input) }.poll_complete(op)
    }
}

/// File content read future result.
struct ReadResult {
    len: usize,
    buffer: Vec<u8>,
}

/// A `SeqRead` adapter for a specific range inside another reader, with a temporary buffer due
/// to lifetime constraints.
#[doc(hidden)]
pub struct SeqReadAtAdapter<T> {
    input: T,
    range: Range<u64>,
    buffer: Vec<u8>,
    future: Option<Pin<Box<dyn Future<Output = io::Result<ReadResult>> + 'static>>>,
}

// We lose `Send` via the boxed trait object and don't want to force the trait object to
// potentially be more strict than `T`, so we leave it as it is ans implement Send and Sync
// depending on T.
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl<T: Send> Send for SeqReadAtAdapter<T> {}
unsafe impl<T: Sync> Sync for SeqReadAtAdapter<T> {}

impl<T> Drop for SeqReadAtAdapter<T> {
    fn drop(&mut self) {
        // drop order
        self.future = None;
    }
}

impl<T: ReadAt> SeqReadAtAdapter<T> {
    /// Create a new `SeqRead` adapter given a range.
    pub fn new(input: T, range: Range<u64>) -> Self {
        if range.end < range.start {
            panic!("BAD SEQ READ AT ADAPTER");
        }
        Self {
            input,
            range,
            buffer: Vec::new(),
            future: None,
        }
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
        dest: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let len = dest.len().min(self.remaining());
        let dest = &mut dest[..len];

        let this = unsafe { self.get_unchecked_mut() };
        loop {
            match this.future.take() {
                None => {
                    let mut buffer = mem::take(&mut this.buffer);
                    util::scale_read_buffer(&mut buffer, dest.len());

                    // Note that we're pinned and we have a drop-handler which forces self.future
                    // to be dropped before `input`, so putting a reference to self.input into the
                    // future should be ok!
                    let reader = &this.input;

                    let at = this.range.start;
                    let future: Pin<Box<dyn Future<Output = io::Result<ReadResult>>>> =
                        Box::pin(async move {
                            let len = reader.read_at(&mut buffer, at).await?;
                            io::Result::Ok(ReadResult { len, buffer })
                        });
                    // Ditch the self-reference life-time now:
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
                        this.range.start += got as u64;
                        let len = got.min(dest.len());
                        dest[..len].copy_from_slice(&this.buffer[..len]);
                        return Poll::Ready(Ok(len));
                    }
                },
            }
        }
    }

    fn poll_position(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Option<io::Result<u64>>> {
        Poll::Ready(Some(Ok(self.range.start)))
    }
}
