//! The `pxar` decoder state machine.
//!
//! This is the implementation used by both the synchronous and async pxar wrappers.

#![deny(missing_docs)]

use std::ffi::OsString;
use std::io;
use std::mem::{self, size_of, size_of_val, MaybeUninit};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

//use std::os::unix::fs::FileExt;

use endian_trait::Endian;

use crate::format::{self, Header};
use crate::poll_fn::poll_fn;
use crate::util::{self, io_err_other};
use crate::{Entry, EntryKind, Metadata};

pub mod aio;
pub mod sync;

#[doc(inline)]
pub use sync::Decoder;

/// To skip through non-seekable files.
static mut SCRATCH_BUFFER: MaybeUninit<[u8; 4096]> = MaybeUninit::uninit();

fn scratch_buffer() -> &'static mut [u8] {
    unsafe { &mut (*SCRATCH_BUFFER.as_mut_ptr())[..] }
}

/// Sequential read interface used by the decoder's state machine.
///
/// To simply iterate through a directory we just need the equivalent of `poll_read()`.
///
/// Currently we also have a `poll_position()` method which can be added for types supporting
/// `Seek` or `AsyncSeek`. In this case the starting position of each entry becomes available
/// (accessible via the `Entry::offset()`), to allow jumping between entries.
pub trait SeqRead {
    /// Mostly we want to read sequentially, so this is basically an `AsyncRead` equivalent.
    fn poll_seq_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>>;

    /// While going through the data we may want to take notes about some offsets within the file
    /// for later. If the reader does not support seeking or positional reading, this can just
    /// return `None`.
    fn poll_position(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Option<io::Result<u64>>> {
        Poll::Ready(None)
    }
}

/// Allow using trait objects for generics taking a `SeqRead`:
impl<'a> SeqRead for &mut (dyn SeqRead + 'a) {
    fn poll_seq_read(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        unsafe {
            self.map_unchecked_mut(|this| &mut **this)
                .poll_seq_read(cx, buf)
        }
    }

    fn poll_position(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<io::Result<u64>>> {
        unsafe { self.map_unchecked_mut(|this| &mut **this).poll_position(cx) }
    }
}

/// awaitable version of `poll_position`.
async fn seq_read_position<T: SeqRead + ?Sized>(input: &mut T) -> Option<io::Result<u64>> {
    poll_fn(|cx| unsafe { Pin::new_unchecked(&mut *input).poll_position(cx) }).await
}

/// awaitable version of `poll_seq_read`.
pub(crate) async fn seq_read<T: SeqRead + ?Sized>(
    input: &mut T,
    buf: &mut [u8],
) -> io::Result<usize> {
    poll_fn(|cx| unsafe { Pin::new_unchecked(&mut *input).poll_seq_read(cx, buf) }).await
}

/// `read_exact` - since that's what we _actually_ want most of the time, but with EOF handling
async fn seq_read_exact_or_eof<T>(input: &mut T, mut buf: &mut [u8]) -> io::Result<Option<()>>
where
    T: SeqRead + ?Sized,
{
    let mut eof_ok = true;
    while !buf.is_empty() {
        match seq_read(&mut *input, buf).await? {
            0 if eof_ok => return Ok(None),
            0 => io_bail!("unexpected EOF"),
            got => buf = &mut buf[got..],
        }
        eof_ok = false;
    }
    Ok(Some(()))
}

/// `read_exact` - since that's what we _actually_ want most of the time.
async fn seq_read_exact<T: SeqRead + ?Sized>(input: &mut T, buf: &mut [u8]) -> io::Result<()> {
    match seq_read_exact_or_eof(input, buf).await? {
        Some(()) => Ok(()),
        None => io_bail!("unexpected EOF"),
    }
}

/// Helper to read into an allocated byte vector.
async fn seq_read_exact_data<T>(input: &mut T, size: usize) -> io::Result<Vec<u8>>
where
    T: SeqRead + ?Sized,
{
    let mut data = unsafe { util::vec_new_uninitialized(size) };
    seq_read_exact(input, &mut data[..]).await?;
    Ok(data)
}

/// `seq_read_entry` with EOF handling
async fn seq_read_entry_or_eof<T, E>(input: &mut T) -> io::Result<Option<E>>
where
    T: SeqRead + ?Sized,
    E: Endian,
{
    let mut data = MaybeUninit::<E>::uninit();
    let buf =
        unsafe { std::slice::from_raw_parts_mut(data.as_mut_ptr() as *mut u8, size_of::<E>()) };
    if seq_read_exact_or_eof(input, buf).await?.is_none() {
        return Ok(None);
    }
    Ok(Some(unsafe { data.assume_init().from_le() }))
}

/// Helper to read into an `Endian`-implementing `struct`.
async fn seq_read_entry<T: SeqRead + ?Sized, E: Endian>(input: &mut T) -> io::Result<E> {
    seq_read_entry_or_eof(input)
        .await?
        .ok_or_else(|| io_format_err!("unexpected EOF"))
}

/// The decoder state machine implementation.
///
/// We use `async fn` to implement the decoder state machine so that we can easily plug in both
/// synchronous or `async` I/O objects in as input.
pub(crate) struct DecoderImpl<T> {
    pub(crate) input: T,
    current_header: Header,
    entry: Entry,
    path_lengths: Vec<usize>,
    state: State,
    with_goodbye_tables: bool,

    /// The random access code uses decoders for sub-ranges which may not end in a `PAYLOAD` for
    /// entries like FIFOs or sockets, so there we explicitly allow an item to terminate with EOF.
    eof_after_entry: bool,
}

enum State {
    Begin,
    Default,
    InPayload {
        offset: u64,
    },

    /// file entries with no data (fifo, socket)
    InSpecialFile,

    InGoodbyeTable,
    InDirectory,
    Eof,
}

/// Control flow while parsing items.
///
/// When parsing an entry, we usually go through all of its attribute items. Once we reach the end
/// of the entry we stop.
/// Note that if we're in a directory, we stopped at the beginning of its contents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ItemResult {
    /// We parsed an "attribute" item and should continue parsing.
    Attribute,

    /// We finished an entry (`SYMLINK`, `HARDLINK`, ...) or just entered the contents of a
    /// directory (`FILENAME`, `GOODBYE`).
    ///
    /// We stop moving forward at this point.
    Entry,
}

impl<I: SeqRead> DecoderImpl<I> {
    pub async fn new(input: I) -> io::Result<Self> {
        Self::new_full(input, "/".into(), false).await
    }

    pub(crate) fn input(&self) -> &I {
        &self.input
    }

    pub(crate) async fn new_full(
        input: I,
        path: PathBuf,
        eof_after_entry: bool,
    ) -> io::Result<Self> {
        let this = DecoderImpl {
            input,
            current_header: unsafe { mem::zeroed() },
            entry: Entry {
                path,
                kind: EntryKind::GoodbyeTable,
                metadata: Metadata::default(),
            },
            path_lengths: Vec::new(),
            state: State::Begin,
            with_goodbye_tables: false,
            eof_after_entry,
        };

        // this.read_next_entry().await?;

        Ok(this)
    }

    /// Get the next file entry, recursing into directories.
    pub async fn next(&mut self) -> Option<io::Result<Entry>> {
        self.next_do().await.transpose()
    }

    async fn next_do(&mut self) -> io::Result<Option<Entry>> {
        loop {
            match self.state {
                State::Eof => return Ok(None),
                State::Begin => return self.read_next_entry().await.map(Some),
                State::Default => {
                    // we completely finished an entry, so now we're going "up" in the directory
                    // hierarchy and parse the next PXAR_FILENAME or the PXAR_GOODBYE:
                    self.read_next_item().await?;
                }
                State::InPayload { offset } => {
                    // We need to skip the current payload first.
                    self.skip_entry(offset).await?;
                    self.read_next_item().await?;
                }
                State::InGoodbyeTable => {
                    self.skip_entry(0).await?;
                    if self.path_lengths.pop().is_none() {
                        // The root directory has an entry containing '1'.
                        io_bail!("unexpected EOF in goodbye table");
                    }

                    if self.path_lengths.is_empty() {
                        // we are at the end of the archive now
                        self.state = State::Eof;
                        return Ok(None);
                    }

                    // We left the directory, now keep going in our parent.
                    self.state = State::Default;
                    continue;
                }
                State::InSpecialFile => {
                    self.entry.clear_data();
                    self.state = State::InDirectory;
                    self.entry.kind = EntryKind::Directory;
                }
                State::InDirectory => {
                    // We're at the next FILENAME or GOODBYE item.
                }
            }

            match self.current_header.htype {
                format::PXAR_FILENAME => return self.handle_file_entry().await,
                format::PXAR_GOODBYE => {
                    self.state = State::InGoodbyeTable;

                    if self.with_goodbye_tables {
                        self.entry.clear_data();
                        return Ok(Some(Entry {
                            path: PathBuf::new(),
                            metadata: Metadata::default(),
                            kind: EntryKind::GoodbyeTable,
                        }));
                    } else {
                        // go up to goodbye table handling
                        continue;
                    }
                }
                _ => io_bail!(
                    "expected filename or directory-goodbye pxar entry, got: {}",
                    self.current_header,
                ),
            }
        }
    }

    pub fn content_size(&self) -> Option<u64> {
        if let State::InPayload { .. } = self.state {
            Some(self.current_header.content_size())
        } else {
            None
        }
    }

    pub fn content_reader(&mut self) -> Option<Contents<I>> {
        if let State::InPayload { offset } = &mut self.state {
            Some(Contents::new(
                &mut self.input,
                offset,
                self.current_header.content_size(),
            ))
        } else {
            None
        }
    }

    async fn handle_file_entry(&mut self) -> io::Result<Option<Entry>> {
        let mut data = self.read_entry_as_bytes().await?;

        // filenames are zero terminated!
        if data.pop() != Some(0) {
            io_bail!("illegal path found (missing terminating zero)");
        }

        crate::util::validate_filename(&data)?;

        let path = PathBuf::from(OsString::from_vec(data));
        self.set_path(&path)?;
        self.read_next_entry().await.map(Some)
    }

    fn reset_path(&mut self) -> io::Result<()> {
        let path_len = *self
            .path_lengths
            .last()
            .ok_or_else(|| io_format_err!("internal decoder error: path underrun"))?;
        let mut path = mem::replace(&mut self.entry.path, PathBuf::new())
            .into_os_string()
            .into_vec();
        path.truncate(path_len);
        self.entry.path = PathBuf::from(OsString::from_vec(path));
        Ok(())
    }

    fn set_path(&mut self, path: &Path) -> io::Result<()> {
        self.reset_path()?;
        self.entry.path.push(path);
        Ok(())
    }

    async fn read_next_entry_or_eof(&mut self) -> io::Result<Option<Entry>> {
        self.state = State::Default;
        self.entry.clear_data();

        let header: Header = match seq_read_entry_or_eof(&mut self.input).await? {
            None => return Ok(None),
            Some(header) => header,
        };

        header.check_header_size()?;

        if header.htype == format::PXAR_HARDLINK {
            // The only "dangling" header without an 'Entry' in front of it because it does not
            // carry its own metadata.
            self.current_header = header;

            // Hardlinks have no metadata and no additional items.
            self.entry.metadata = Metadata::default();
            self.entry.kind = EntryKind::Hardlink(self.read_hardlink().await?);

            Ok(Some(self.entry.take()))
        } else if header.htype == format::PXAR_ENTRY || header.htype == format::PXAR_ENTRY_V1 {
            if header.htype == format::PXAR_ENTRY {
                self.entry.metadata = Metadata {
                    stat: seq_read_entry(&mut self.input).await?,
                    ..Default::default()
                };
            } else if header.htype == format::PXAR_ENTRY_V1 {
                let stat: format::Stat_V1 = seq_read_entry(&mut self.input).await?;

                self.entry.metadata = Metadata {
                    stat: stat.into(),
                    ..Default::default()
                };
            } else {
                unreachable!();
            }

            self.current_header = unsafe { mem::zeroed() };

            loop {
                match self.read_next_item_or_eof().await? {
                    Some(ItemResult::Entry) => break,
                    Some(ItemResult::Attribute) => continue,
                    None if self.eof_after_entry => {
                        // Single FIFOs and sockets (as received from the Accessor) won't reach a
                        // FILENAME/GOODBYE entry:
                        if self.entry.metadata.is_fifo() {
                            self.entry.kind = EntryKind::Fifo;
                        } else if self.entry.metadata.is_socket() {
                            self.entry.kind = EntryKind::Socket;
                        } else {
                            self.entry.kind = EntryKind::Directory;
                        }
                        break;
                    }
                    None => io_bail!("unexpected EOF in entry"),
                }
            }

            if self.entry.is_dir() {
                self.path_lengths
                    .push(self.entry.path.as_os_str().as_bytes().len());
            }

            Ok(Some(self.entry.take()))
        } else {
            io_bail!("expected pxar entry of type 'Entry', got: {}", header,);
        }
    }

    async fn read_next_entry(&mut self) -> io::Result<Entry> {
        self.read_next_entry_or_eof()
            .await?
            .ok_or_else(|| io_format_err!("unexpected EOF"))
    }

    async fn read_next_item(&mut self) -> io::Result<ItemResult> {
        match self.read_next_item_or_eof().await? {
            Some(item) => Ok(item),
            None => io_bail!("unexpected EOF"),
        }
    }

    // NOTE: The random accessor will decode FIFOs and Sockets in a decoder instance with a ranged
    // reader so there is no PAYLOAD or GOODBYE TABLE to "end" an entry.
    //
    // NOTE: This behavior method is also recreated in the accessor's `get_decoder_at_filename`
    // function! Keep in mind when changing!
    async fn read_next_item_or_eof(&mut self) -> io::Result<Option<ItemResult>> {
        match self.read_next_header_or_eof().await? {
            Some(()) => self.read_current_item().await.map(Some),
            None => Ok(None),
        }
    }

    async fn read_next_header_or_eof(&mut self) -> io::Result<Option<()>> {
        let dest = unsafe {
            std::slice::from_raw_parts_mut(
                &mut self.current_header as *mut Header as *mut u8,
                size_of_val(&self.current_header),
            )
        };

        match seq_read_exact_or_eof(&mut self.input, dest).await? {
            Some(()) => {
                self.current_header.check_header_size()?;
                Ok(Some(()))
            }
            None => Ok(None),
        }
    }

    /// Read the next item, the header is already loaded.
    async fn read_current_item(&mut self) -> io::Result<ItemResult> {
        match self.current_header.htype {
            format::PXAR_XATTR => {
                let xattr = self.read_xattr().await?;
                self.entry.metadata.xattrs.push(xattr);
            }
            format::PXAR_ACL_USER => {
                let entry = self.read_acl_user().await?;
                self.entry.metadata.acl.users.push(entry);
            }
            format::PXAR_ACL_GROUP => {
                let entry = self.read_acl_group().await?;
                self.entry.metadata.acl.groups.push(entry);
            }
            format::PXAR_ACL_GROUP_OBJ => {
                if self.entry.metadata.acl.group_obj.is_some() {
                    io_bail!("multiple acl group object entries detected");
                }
                let entry = self.read_acl_group_object().await?;
                self.entry.metadata.acl.group_obj = Some(entry);
            }
            format::PXAR_ACL_DEFAULT => {
                if self.entry.metadata.acl.default.is_some() {
                    io_bail!("multiple acl default entries detected");
                }
                let entry = self.read_acl_default().await?;
                self.entry.metadata.acl.default = Some(entry);
            }
            format::PXAR_ACL_DEFAULT_USER => {
                let entry = self.read_acl_user().await?;
                self.entry.metadata.acl.default_users.push(entry);
            }
            format::PXAR_ACL_DEFAULT_GROUP => {
                let entry = self.read_acl_group().await?;
                self.entry.metadata.acl.default_groups.push(entry);
            }
            format::PXAR_FCAPS => {
                if self.entry.metadata.fcaps.is_some() {
                    io_bail!("multiple file capability entries detected");
                }
                let entry = self.read_fcaps().await?;
                self.entry.metadata.fcaps = Some(entry);
            }
            format::PXAR_QUOTA_PROJID => {
                if self.entry.metadata.quota_project_id.is_some() {
                    io_bail!("multiple quota project id entries detected");
                }
                let entry = self.read_quota_project_id().await?;
                self.entry.metadata.quota_project_id = Some(entry);
            }
            format::PXAR_SYMLINK => {
                self.entry.kind = EntryKind::Symlink(self.read_symlink().await?);
                return Ok(ItemResult::Entry);
            }
            format::PXAR_HARDLINK => io_bail!("encountered unexpected hardlink entry"),
            format::PXAR_DEVICE => {
                self.entry.kind = EntryKind::Device(self.read_device().await?);
                return Ok(ItemResult::Entry);
            }
            format::PXAR_PAYLOAD => {
                let offset = seq_read_position(&mut self.input).await.transpose()?;
                self.entry.kind = EntryKind::File {
                    size: self.current_header.content_size(),
                    offset,
                };
                self.state = State::InPayload { offset: 0 };
                return Ok(ItemResult::Entry);
            }
            format::PXAR_FILENAME | format::PXAR_GOODBYE => {
                if self.entry.metadata.is_fifo() {
                    self.state = State::InSpecialFile;
                    self.entry.kind = EntryKind::Fifo;
                } else if self.entry.metadata.is_socket() {
                    self.state = State::InSpecialFile;
                    self.entry.kind = EntryKind::Socket;
                } else {
                    // As a shortcut this is copy-pasted to `next_do`'s `InSpecialFile` case.
                    // Keep in mind when editing this!
                    self.state = State::InDirectory;
                    self.entry.kind = EntryKind::Directory;
                }
                return Ok(ItemResult::Entry);
            }
            _ => io_bail!("unexpected entry type: {}", self.current_header),
        }

        Ok(ItemResult::Attribute)
    }

    //
    // Local read helpers.
    //
    // These utilize additional information and hence are not part of the `dyn SeqRead` impl.
    //

    async fn skip_entry(&mut self, offset: u64) -> io::Result<()> {
        let mut len = self.current_header.content_size() - offset;
        let scratch = scratch_buffer();
        while len >= (scratch.len() as u64) {
            seq_read_exact(&mut self.input, scratch).await?;
            len -= scratch.len() as u64;
        }
        let len = len as usize;
        if len > 0 {
            seq_read_exact(&mut self.input, &mut scratch[..len]).await?;
        }
        Ok(())
    }

    async fn read_entry_as_bytes(&mut self) -> io::Result<Vec<u8>> {
        let size = usize::try_from(self.current_header.content_size()).map_err(io_err_other)?;
        let data = seq_read_exact_data(&mut self.input, size).await?;
        Ok(data)
    }

    /// Helper to read a struct entry while checking its size.
    async fn read_simple_entry<T: Endian + 'static>(
        &mut self,
        what: &'static str,
    ) -> io::Result<T> {
        if self.current_header.content_size() != (size_of::<T>() as u64) {
            io_bail!(
                "bad {} size: {} (expected {})",
                what,
                self.current_header.content_size(),
                size_of::<T>(),
            );
        }
        seq_read_entry(&mut self.input).await
    }

    //
    // Read functions for PXAR components.
    //

    async fn read_xattr(&mut self) -> io::Result<format::XAttr> {
        let data = self.read_entry_as_bytes().await?;

        let name_len = data
            .iter()
            .position(|c| *c == 0)
            .ok_or_else(|| io_format_err!("missing value separator in xattr"))?;

        Ok(format::XAttr { data, name_len })
    }

    async fn read_symlink(&mut self) -> io::Result<format::Symlink> {
        let data = self.read_entry_as_bytes().await?;
        Ok(format::Symlink { data })
    }

    async fn read_hardlink(&mut self) -> io::Result<format::Hardlink> {
        let content_size =
            usize::try_from(self.current_header.content_size()).map_err(io_err_other)?;

        if content_size <= size_of::<u64>() {
            io_bail!("bad hardlink entry (too small)");
        }
        let data_size = content_size - size_of::<u64>();

        let offset: u64 = seq_read_entry(&mut self.input).await?;
        let data = seq_read_exact_data(&mut self.input, data_size).await?;

        Ok(format::Hardlink { offset, data })
    }

    async fn read_device(&mut self) -> io::Result<format::Device> {
        self.read_simple_entry("device").await
    }

    async fn read_fcaps(&mut self) -> io::Result<format::FCaps> {
        let data = self.read_entry_as_bytes().await?;
        Ok(format::FCaps { data })
    }

    async fn read_acl_user(&mut self) -> io::Result<format::acl::User> {
        self.read_simple_entry("acl user").await
    }

    async fn read_acl_group(&mut self) -> io::Result<format::acl::Group> {
        self.read_simple_entry("acl group").await
    }

    async fn read_acl_group_object(&mut self) -> io::Result<format::acl::GroupObject> {
        self.read_simple_entry("acl group object").await
    }

    async fn read_acl_default(&mut self) -> io::Result<format::acl::Default> {
        self.read_simple_entry("acl default").await
    }

    async fn read_quota_project_id(&mut self) -> io::Result<format::QuotaProjectId> {
        self.read_simple_entry("quota project id").await
    }
}

/// Reader for file contents inside a pxar archive.
pub struct Contents<'a, T: SeqRead> {
    input: &'a mut T,
    at: &'a mut u64,
    len: u64,
}

impl<'a, T: SeqRead> Contents<'a, T> {
    fn new(input: &'a mut T, at: &'a mut u64, len: u64) -> Self {
        Self { input, at, len }
    }

    #[inline]
    fn remaining(&self) -> u64 {
        self.len - *self.at
    }
}

impl<'a, T: SeqRead> SeqRead for Contents<'a, T> {
    fn poll_seq_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let max_read = (buf.len() as u64).min(self.remaining()) as usize;
        if max_read == 0 {
            return Poll::Ready(Ok(0));
        }

        let buf = &mut buf[..max_read];
        let got = ready!(unsafe { Pin::new_unchecked(&mut *self.input) }.poll_seq_read(cx, buf))?;
        *self.at += got as u64;
        Poll::Ready(Ok(got))
    }

    fn poll_position(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<io::Result<u64>>> {
        unsafe { Pin::new_unchecked(&mut *self.input) }.poll_position(cx)
    }
}
