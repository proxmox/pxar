//! The `pxar` encoder state machine.
//!
//! This is the implementation used by both the synchronous and async pxar wrappers.

#![deny(missing_docs)]

use std::future::poll_fn;
use std::io;
use std::mem::{forget, size_of, size_of_val, take};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use endian_trait::Endian;

use crate::binary_tree_array;
use crate::decoder::{self, SeqRead};
use crate::format::{self, FormatVersion, GoodbyeItem, PayloadRef};
use crate::{Metadata, PxarVariant};

pub mod aio;
pub mod sync;

#[doc(inline)]
pub use sync::Encoder;

/// File reference used to create hard links.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct LinkOffset(u64);

impl LinkOffset {
    /// Get the raw byte offset of this link.
    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// File reference used to create payload references.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct PayloadOffset(u64);

impl PayloadOffset {
    /// Get the raw byte offset of this link.
    #[inline]
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Return a new PayloadOffset, positively shifted by offset
    #[inline]
    pub fn add(&self, offset: u64) -> Self {
        Self(self.0 + offset)
    }
}

/// Sequential write interface used by the encoder's state machine.
///
/// This is our internal writer trait which is available for `std::io::Write` types in the
/// synchronous wrapper and for both `tokio` and `future` `AsyncWrite` types in the asynchronous
/// wrapper.
pub trait SeqWrite {
    /// Attempt to perform a sequential write to the file. On success, the number of written bytes
    /// is returned as `Poll::Ready(Ok(bytes))`.
    ///
    /// If writing is not yet possible, `Poll::Pending` is returned and the current task will be
    /// notified via the `cx.waker()` when writing becomes possible.
    fn poll_seq_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>>;

    /// Attempt to flush the output, ensuring that all buffered data reaches the destination.
    ///
    /// On success, returns `Poll::Ready(Ok(()))`.
    ///
    /// If flushing cannot complete immediately, `Poll::Pending` is returned and the current task
    /// will be notified via `cx.waker()` when progress can be made.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>>;
}

/// Allow using trait objects for generics taking a `SeqWrite`.
impl<S> SeqWrite for &mut S
where
    S: SeqWrite + ?Sized,
{
    fn poll_seq_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        unsafe {
            self.map_unchecked_mut(|this| &mut **this)
                .poll_seq_write(cx, buf)
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        unsafe { self.map_unchecked_mut(|this| &mut **this).poll_flush(cx) }
    }
}

/// awaitable verison of `poll_seq_write`.
async fn seq_write<T: SeqWrite + ?Sized>(
    output: &mut T,
    buf: &[u8],
    position: &mut u64,
) -> io::Result<usize> {
    let put =
        poll_fn(|cx| unsafe { Pin::new_unchecked(&mut *output).poll_seq_write(cx, buf) }).await?;
    *position += put as u64;
    Ok(put)
}

/// awaitable version of 'poll_flush'.
async fn flush<T: SeqWrite + ?Sized>(output: &mut T) -> io::Result<()> {
    poll_fn(|cx| unsafe { Pin::new_unchecked(&mut *output).poll_flush(cx) }).await
}

/// Write the entire contents of a buffer, handling short writes.
async fn seq_write_all<T: SeqWrite + ?Sized>(
    output: &mut T,
    mut buf: &[u8],
    position: &mut u64,
) -> io::Result<()> {
    while !buf.is_empty() {
        let got = seq_write(&mut *output, buf, &mut *position).await?;
        buf = &buf[got..];
    }
    Ok(())
}

/// Write an endian-swappable struct.
async fn seq_write_struct<E: Endian, T>(
    output: &mut T,
    data: E,
    position: &mut u64,
) -> io::Result<()>
where
    T: SeqWrite + ?Sized,
{
    let data = data.to_le();
    let buf =
        unsafe { std::slice::from_raw_parts(&data as *const E as *const u8, size_of_val(&data)) };
    seq_write_all(output, buf, position).await
}

/// Write a pxar entry.
async fn seq_write_pxar_entry<T>(
    output: &mut T,
    htype: u64,
    data: &[u8],
    position: &mut u64,
) -> io::Result<()>
where
    T: SeqWrite + ?Sized,
{
    let header = format::Header::with_content_size(htype, data.len() as u64);
    header.check_header_size()?;

    seq_write_struct(&mut *output, header, &mut *position).await?;
    seq_write_all(output, data, position).await
}

/// Write a pxar entry terminated by an additional zero which is not contained in the provided
/// data buffer.
async fn seq_write_pxar_entry_zero<T>(
    output: &mut T,
    htype: u64,
    data: &[u8],
    position: &mut u64,
) -> io::Result<()>
where
    T: SeqWrite + ?Sized,
{
    let header = format::Header::with_content_size(htype, 1 + data.len() as u64);
    header.check_header_size()?;

    seq_write_struct(&mut *output, header, &mut *position).await?;
    seq_write_all(&mut *output, data, &mut *position).await?;
    seq_write_all(output, &[0u8], position).await
}

/// Write a pxar entry consisting of an endian-swappable struct.
async fn seq_write_pxar_struct_entry<E, T>(
    output: &mut T,
    htype: u64,
    data: E,
    position: &mut u64,
) -> io::Result<()>
where
    T: SeqWrite + ?Sized,
    E: Endian,
{
    let data = data.to_le();
    let buf =
        unsafe { std::slice::from_raw_parts(&data as *const E as *const u8, size_of_val(&data)) };
    seq_write_pxar_entry(output, htype, buf, position).await
}

/// Error conditions caused by wrong usage of this crate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EncodeError {
    /// The user dropped a `File` without finishing writing all of its contents.
    ///
    /// Completely writing the contents is required because the payload length is written out first
    /// and decoding requires the the encoded length and the following data to match.
    IncompleteFile,

    /// The user dropped a directory without finalizing it.
    ///
    /// Finalizing is required to build the goodbye table at the end of a directory.
    IncompleteDirectory,
}

#[derive(Default)]
struct EncoderState {
    /// Goodbye items for this directory, excluding the tail.
    items: Vec<GoodbyeItem>,

    /// User caused error conditions.
    encode_error: Option<EncodeError>,

    /// Offset of this directory's ENTRY.
    entry_offset: u64,

    #[allow(dead_code)]
    /// Offset to this directory's first FILENAME.
    files_offset: u64,

    /// If this is a subdirectory, this points to the this directory's FILENAME.
    file_offset: Option<u64>,

    /// If this is a subdirectory, this contains this directory's hash for the goodbye item.
    file_hash: u64,

    /// We need to keep track how much we have written to get offsets.
    write_position: u64,

    /// Track the bytes written to the payload writer
    payload_write_position: u64,

    /// Mark the encoder state as correctly finished, ready to be dropped
    finished: bool,
}

impl EncoderState {
    #[inline]
    fn position(&self) -> u64 {
        self.write_position
    }

    #[inline]
    fn payload_position(&self) -> u64 {
        self.payload_write_position
    }

    fn merge_error(&mut self, error: Option<EncodeError>) {
        // one error is enough:
        if self.encode_error.is_none() {
            self.encode_error = error;
        }
    }

    fn add_error(&mut self, error: EncodeError) {
        self.merge_error(Some(error));
    }

    fn finish(&mut self) -> Option<EncodeError> {
        self.finished = true;
        self.encode_error.take()
    }
}

impl Drop for EncoderState {
    fn drop(&mut self) {
        if !self.finished {
            eprintln!("unfinished encoder state dropped");
        }

        if self.encode_error.is_some() {
            eprintln!("finished encoder state with errors");
        }
    }
}

pub(crate) enum EncoderOutput<'a, T> {
    Owned(T),
    Borrowed(&'a mut T),
}

impl<'a, T> std::convert::AsMut<T> for EncoderOutput<'a, T> {
    fn as_mut(&mut self) -> &mut T {
        match self {
            EncoderOutput::Owned(o) => o,
            EncoderOutput::Borrowed(b) => b,
        }
    }
}

impl<'a, T> std::convert::From<T> for EncoderOutput<'a, T> {
    fn from(t: T) -> Self {
        EncoderOutput::Owned(t)
    }
}

impl<'a, T> std::convert::From<&'a mut T> for EncoderOutput<'a, T> {
    fn from(t: &'a mut T) -> Self {
        EncoderOutput::Borrowed(t)
    }
}

/// The encoder state machine implementation for a directory.
///
/// We use `async fn` to implement the encoder state machine so that we can easily plug in both
/// synchronous or `async` I/O objects in as output.
pub(crate) struct EncoderImpl<'a, T: SeqWrite + 'a> {
    output: PxarVariant<EncoderOutput<'a, T>, T>,
    /// EncoderState stack storing the state for each directory level
    state: Vec<EncoderState>,
    finished: bool,

    /// Since only the "current" entry can be actively writing files, we share the file copy
    /// buffer.
    file_copy_buffer: Arc<Mutex<Vec<u8>>>,
    /// Pxar format version to encode
    version: format::FormatVersion,
}

impl<'a, T: SeqWrite + 'a> Drop for EncoderImpl<'a, T> {
    fn drop(&mut self) {
        if !self.finished {
            eprintln!("unclosed encoder dropped");
        }

        if !self.state.is_empty() {
            eprintln!("closed encoder dropped with state");
        }
    }
}

impl<'a, T: SeqWrite + 'a> EncoderImpl<'a, T> {
    pub async fn new(
        mut output: PxarVariant<EncoderOutput<'a, T>, T>,
        metadata: &Metadata,
        prelude: Option<&[u8]>,
    ) -> io::Result<EncoderImpl<'a, T>> {
        if !metadata.is_dir() {
            io_bail!("directory metadata must contain the directory mode flag");
        }

        let mut state = EncoderState::default();
        let version = if let Some(payload_output) = output.payload_mut() {
            let header = format::Header::with_content_size(format::PXAR_PAYLOAD_START_MARKER, 0);
            header.check_header_size()?;
            seq_write_struct(payload_output, header, &mut state.payload_write_position).await?;
            format::FormatVersion::Version2
        } else {
            format::FormatVersion::Version1
        };

        let mut this = Self {
            output,
            state: vec![state],
            finished: false,
            file_copy_buffer: Arc::new(Mutex::new(unsafe {
                crate::util::vec_new_uninitialized(1024 * 1024)
            })),
            version,
        };

        this.encode_format_version().await?;
        if let Some(prelude) = prelude {
            this.encode_prelude(prelude).await?;
        }
        this.encode_metadata(metadata).await?;
        let state = this.state_mut()?;
        state.files_offset = state.position();

        Ok(this)
    }

    fn check(&self) -> io::Result<()> {
        if self.finished {
            io_bail!("unexpected encoder finished state");
        }
        let state = self.state()?;
        match state.encode_error {
            Some(EncodeError::IncompleteFile) => io_bail!("incomplete file"),
            Some(EncodeError::IncompleteDirectory) => io_bail!("directory not finalized"),
            None => Ok(()),
        }
    }

    fn state(&self) -> io::Result<&EncoderState> {
        self.state
            .last()
            .ok_or_else(|| io_format_err!("encoder state stack underflow"))
    }

    fn state_mut(&mut self) -> io::Result<&mut EncoderState> {
        self.state
            .last_mut()
            .ok_or_else(|| io_format_err!("encoder state stack underflow"))
    }

    fn output_state(&mut self) -> io::Result<(PxarVariant<&mut T, &mut T>, &mut EncoderState)> {
        let output = match &mut self.output {
            PxarVariant::Unified(output) => PxarVariant::Unified(output.as_mut()),
            PxarVariant::Split(output, payload_output) => {
                PxarVariant::Split(output.as_mut(), payload_output)
            }
        };

        Ok((
            output,
            self.state
                .last_mut()
                .ok_or_else(|| io_format_err!("encoder state stack underflow"))?,
        ))
    }

    pub async fn create_file<'b>(
        &'b mut self,
        metadata: &Metadata,
        file_name: &Path,
        file_size: u64,
    ) -> io::Result<FileImpl<'b, T>>
    where
        'a: 'b,
    {
        self.create_file_do(metadata, file_name.as_os_str().as_bytes(), file_size)
            .await
    }

    async fn create_file_do<'b>(
        &'b mut self,
        metadata: &Metadata,
        file_name: &[u8],
        file_size: u64,
    ) -> io::Result<FileImpl<'b, T>>
    where
        'a: 'b,
    {
        self.check()?;

        let file_offset = self.state()?.position();
        self.start_file_do(Some(metadata), file_name).await?;

        let (mut output, state) = self.output_state()?;
        if let Some(payload_output) = output.payload_mut() {
            // payload references must point to the position prior to the payload header,
            // separating payload entries in the payload stream
            let payload_position = state.payload_position();

            let header = format::Header::with_content_size(format::PXAR_PAYLOAD, file_size);
            header.check_header_size()?;
            seq_write_struct(payload_output, header, &mut state.payload_write_position).await?;

            let payload_ref = PayloadRef {
                offset: payload_position,
                size: file_size,
            };

            seq_write_pxar_entry(
                output.archive_mut(),
                format::PXAR_PAYLOAD_REF,
                &payload_ref.data(),
                &mut state.write_position,
            )
            .await?;
        } else {
            let header = format::Header::with_content_size(format::PXAR_PAYLOAD, file_size);
            header.check_header_size()?;
            seq_write_struct(output.archive_mut(), header, &mut state.write_position).await?;
        }

        let payload_data_offset = state.position();

        let meta_size = payload_data_offset - file_offset;

        Ok(FileImpl {
            output,
            goodbye_item: GoodbyeItem {
                hash: format::hash_filename(file_name),
                offset: file_offset,
                size: file_size + meta_size,
            },
            remaining_size: file_size,
            parent: state,
        })
    }

    fn take_file_copy_buffer(&self) -> Vec<u8> {
        let buf: Vec<_> = take(
            &mut self
                .file_copy_buffer
                .lock()
                .expect("failed to lock temporary buffer mutex"),
        );
        if buf.len() < 1024 * 1024 {
            drop(buf);
            unsafe { crate::util::vec_new_uninitialized(1024 * 1024) }
        } else {
            buf
        }
    }

    fn put_file_copy_buffer(&self, buf: Vec<u8>) {
        let mut lock = self
            .file_copy_buffer
            .lock()
            .expect("failed to lock temporary buffer mutex");
        if lock.len() < buf.len() {
            *lock = buf;
        }
    }

    /// Return a file offset usable with `add_hardlink`.
    pub async fn add_file(
        &mut self,
        metadata: &Metadata,
        file_name: &Path,
        file_size: u64,
        content: &mut dyn SeqRead,
    ) -> io::Result<LinkOffset> {
        let mut buf = self.take_file_copy_buffer();
        let mut file = self.create_file(metadata, file_name, file_size).await?;
        loop {
            let got = decoder::seq_read(&mut *content, &mut buf[..]).await?;
            if got == 0 {
                break;
            } else {
                file.write_all(&buf[..got]).await?;
            }
        }
        let offset = file.file_offset();
        drop(file);
        self.put_file_copy_buffer(buf);
        Ok(offset)
    }

    pub fn payload_position(&self) -> io::Result<PayloadOffset> {
        Ok(PayloadOffset(self.state()?.payload_position()))
    }

    /// Encode a payload reference pointing to given offset in the separate payload output
    ///
    /// Returns a file offset usable with `add_hardlink` or with error if the encoder instance has
    /// no separate payload output or encoding failed.
    pub async fn add_payload_ref(
        &mut self,
        metadata: &Metadata,
        file_name: &Path,
        file_size: u64,
        payload_offset: PayloadOffset,
    ) -> io::Result<LinkOffset> {
        if self.version == FormatVersion::Version1 {
            io_bail!("payload references not supported in format version 1");
        }

        if self.output.payload().is_none() {
            io_bail!("unable to add payload reference");
        }

        let offset = payload_offset.raw();
        let payload_position = self.state()?.payload_position();
        if offset < payload_position {
            io_bail!("offset smaller than current position: {offset} < {payload_position}");
        }

        let payload_ref = PayloadRef {
            offset,
            size: file_size,
        };
        let this_offset: LinkOffset = self
            .add_file_entry(
                Some(metadata),
                file_name,
                Some((format::PXAR_PAYLOAD_REF, &payload_ref.data())),
            )
            .await?;

        Ok(this_offset)
    }

    /// Add size to payload stream
    pub fn advance(&mut self, size: PayloadOffset) -> io::Result<()> {
        self.state_mut()?.payload_write_position += size.raw();
        Ok(())
    }

    /// Return a file offset usable with `add_hardlink`.
    pub async fn add_symlink(
        &mut self,
        metadata: &Metadata,
        file_name: &Path,
        target: &Path,
    ) -> io::Result<()> {
        let target = target.as_os_str().as_bytes();
        let mut data = Vec::with_capacity(target.len() + 1);
        data.extend(target);
        data.push(0);
        let _ofs: LinkOffset = self
            .add_file_entry(
                Some(metadata),
                file_name,
                Some((format::PXAR_SYMLINK, &data)),
            )
            .await?;
        Ok(())
    }

    /// Return a file offset usable with `add_hardlink`.
    pub async fn add_hardlink(
        &mut self,
        file_name: &Path,
        target: &Path,
        target_offset: LinkOffset,
    ) -> io::Result<()> {
        let current_offset = self.state()?.position();
        if current_offset <= target_offset.0 {
            io_bail!("invalid hardlink offset, can only point to prior files");
        }

        let offset_bytes = (current_offset - target_offset.0).to_le_bytes();
        let target_bytes = target.as_os_str().as_bytes();
        let mut hardlink = Vec::with_capacity(offset_bytes.len() + target_bytes.len() + 1);
        hardlink.extend(offset_bytes);
        hardlink.extend(target_bytes);
        hardlink.push(0);
        let _this_offset: LinkOffset = self
            .add_file_entry(None, file_name, Some((format::PXAR_HARDLINK, &hardlink)))
            .await?;
        Ok(())
    }

    /// Return a file offset usable with `add_hardlink`.
    pub async fn add_device(
        &mut self,
        metadata: &Metadata,
        file_name: &Path,
        device: format::Device,
    ) -> io::Result<()> {
        if !metadata.is_device() {
            io_bail!("entry added via add_device must have a device mode in its metadata");
        }

        let device = device.to_le();
        let device = unsafe {
            std::slice::from_raw_parts(
                &device as *const format::Device as *const u8,
                size_of::<format::Device>(),
            )
        };
        let _ofs: LinkOffset = self
            .add_file_entry(
                Some(metadata),
                file_name,
                Some((format::PXAR_DEVICE, device)),
            )
            .await?;
        Ok(())
    }

    /// Return a file offset usable with `add_hardlink`.
    pub async fn add_fifo(&mut self, metadata: &Metadata, file_name: &Path) -> io::Result<()> {
        if !metadata.is_fifo() {
            io_bail!("entry added via add_device must be of type fifo in its metadata");
        }

        let _ofs: LinkOffset = self.add_file_entry(Some(metadata), file_name, None).await?;
        Ok(())
    }

    /// Return a file offset usable with `add_hardlink`.
    pub async fn add_socket(&mut self, metadata: &Metadata, file_name: &Path) -> io::Result<()> {
        if !metadata.is_socket() {
            io_bail!("entry added via add_device must be of type socket in its metadata");
        }

        let _ofs: LinkOffset = self.add_file_entry(Some(metadata), file_name, None).await?;
        Ok(())
    }

    /// Return a file offset usable with `add_hardlink`.
    async fn add_file_entry(
        &mut self,
        metadata: Option<&Metadata>,
        file_name: &Path,
        entry_htype_data: Option<(u64, &[u8])>,
    ) -> io::Result<LinkOffset> {
        self.check()?;

        let file_offset = self.state()?.position();

        let file_name = file_name.as_os_str().as_bytes();

        self.start_file_do(metadata, file_name).await?;

        let (mut output, state) = self.output_state()?;
        if let Some((htype, entry_data)) = entry_htype_data {
            seq_write_pxar_entry(
                output.archive_mut(),
                htype,
                entry_data,
                &mut state.write_position,
            )
            .await?;
        }

        let end_offset = state.position();

        state.items.push(GoodbyeItem {
            hash: format::hash_filename(file_name),
            offset: file_offset,
            size: end_offset - file_offset,
        });

        Ok(LinkOffset(file_offset))
    }

    pub async fn create_directory(
        &mut self,
        file_name: &Path,
        metadata: &Metadata,
    ) -> io::Result<()> {
        self.check()?;

        if !metadata.is_dir() {
            io_bail!("directory metadata must contain the directory mode flag");
        }

        let file_name = file_name.as_os_str().as_bytes();
        let file_hash = format::hash_filename(file_name);

        let file_offset = self.state()?.position();
        self.encode_filename(file_name).await?;

        let entry_offset = self.state()?.position();
        self.encode_metadata(metadata).await?;

        let state = self.state_mut()?;
        let files_offset = state.position();

        // the child will write to OUR state now:
        let write_position = state.position();
        let payload_write_position = state.payload_position();

        self.state.push(EncoderState {
            items: Vec::new(),
            encode_error: None,
            entry_offset,
            files_offset,
            file_offset: Some(file_offset),
            file_hash,
            write_position,
            payload_write_position,
            finished: false,
        });

        Ok(())
    }

    async fn start_file_do(
        &mut self,
        metadata: Option<&Metadata>,
        file_name: &[u8],
    ) -> io::Result<()> {
        self.encode_filename(file_name).await?;
        if let Some(metadata) = metadata {
            self.encode_metadata(metadata).await?;
        }
        Ok(())
    }

    async fn encode_prelude(&mut self, prelude: &[u8]) -> io::Result<()> {
        if self.version == FormatVersion::Version1 {
            io_bail!("encoding prelude not supported in format version 1");
        }

        let (mut output, state) = self.output_state()?;
        if state.write_position != (size_of::<u64>() + size_of::<format::Header>()) as u64 {
            io_bail!(
                "prelude must be encoded following the version header, current position {}",
                state.write_position,
            );
        }

        seq_write_pxar_entry(
            output.archive_mut(),
            format::PXAR_PRELUDE,
            prelude,
            &mut state.write_position,
        )
        .await
    }

    async fn encode_format_version(&mut self) -> io::Result<()> {
        if let Some(version_bytes) = self.version.serialize() {
            let (mut output, state) = self.output_state()?;
            if state.write_position != 0 {
                io_bail!("format version must be encoded at the beginning of an archive");
            }

            return seq_write_pxar_entry(
                output.archive_mut(),
                format::PXAR_FORMAT_VERSION,
                &version_bytes,
                &mut state.write_position,
            )
            .await;
        }

        Ok(())
    }

    async fn encode_metadata(&mut self, metadata: &Metadata) -> io::Result<()> {
        let (mut output, state) = self.output_state()?;
        seq_write_pxar_struct_entry(
            output.archive_mut(),
            format::PXAR_ENTRY,
            metadata.stat.clone(),
            &mut state.write_position,
        )
        .await?;

        for xattr in &metadata.xattrs {
            self.write_xattr(xattr).await?;
        }

        self.write_acls(&metadata.acl).await?;

        if let Some(fcaps) = &metadata.fcaps {
            self.write_file_capabilities(fcaps).await?;
        }

        if let Some(qpid) = &metadata.quota_project_id {
            self.write_quota_project_id(qpid).await?;
        }

        Ok(())
    }

    async fn write_xattr(&mut self, xattr: &format::XAttr) -> io::Result<()> {
        let (mut output, state) = self.output_state()?;
        seq_write_pxar_entry(
            output.archive_mut(),
            format::PXAR_XATTR,
            &xattr.data,
            &mut state.write_position,
        )
        .await
    }

    async fn write_acls(&mut self, acl: &crate::Acl) -> io::Result<()> {
        let (mut output, state) = self.output_state()?;
        for acl in &acl.users {
            seq_write_pxar_struct_entry(
                output.archive_mut(),
                format::PXAR_ACL_USER,
                acl.clone(),
                &mut state.write_position,
            )
            .await?;
        }

        for acl in &acl.groups {
            seq_write_pxar_struct_entry(
                output.archive_mut(),
                format::PXAR_ACL_GROUP,
                acl.clone(),
                &mut state.write_position,
            )
            .await?;
        }

        if let Some(acl) = &acl.group_obj {
            seq_write_pxar_struct_entry(
                output.archive_mut(),
                format::PXAR_ACL_GROUP_OBJ,
                acl.clone(),
                &mut state.write_position,
            )
            .await?;
        }

        if let Some(acl) = &acl.default {
            seq_write_pxar_struct_entry(
                output.archive_mut(),
                format::PXAR_ACL_DEFAULT,
                acl.clone(),
                &mut state.write_position,
            )
            .await?;
        }

        for acl in &acl.default_users {
            seq_write_pxar_struct_entry(
                output.archive_mut(),
                format::PXAR_ACL_DEFAULT_USER,
                acl.clone(),
                &mut state.write_position,
            )
            .await?;
        }

        for acl in &acl.default_groups {
            seq_write_pxar_struct_entry(
                output.archive_mut(),
                format::PXAR_ACL_DEFAULT_GROUP,
                acl.clone(),
                &mut state.write_position,
            )
            .await?;
        }

        Ok(())
    }

    async fn write_file_capabilities(&mut self, fcaps: &format::FCaps) -> io::Result<()> {
        let (mut output, state) = self.output_state()?;
        seq_write_pxar_entry(
            output.archive_mut(),
            format::PXAR_FCAPS,
            &fcaps.data,
            &mut state.write_position,
        )
        .await
    }

    async fn write_quota_project_id(
        &mut self,
        quota_project_id: &format::QuotaProjectId,
    ) -> io::Result<()> {
        let (mut output, state) = self.output_state()?;
        seq_write_pxar_struct_entry(
            output.archive_mut(),
            format::PXAR_QUOTA_PROJID,
            *quota_project_id,
            &mut state.write_position,
        )
        .await
    }

    async fn encode_filename(&mut self, file_name: &[u8]) -> io::Result<()> {
        crate::util::validate_filename(file_name)?;
        let (mut output, state) = self.output_state()?;
        seq_write_pxar_entry_zero(
            output.archive_mut(),
            format::PXAR_FILENAME,
            file_name,
            &mut state.write_position,
        )
        .await
    }

    pub async fn close(mut self) -> io::Result<()> {
        if !self.state.is_empty() {
            io_bail!("unexpected state on encoder close");
        }

        if let Some(output) = self.output.payload_mut() {
            let mut dummy_writer = 0;
            seq_write_pxar_entry(
                output,
                format::PXAR_PAYLOAD_TAIL_MARKER,
                &[],
                &mut dummy_writer,
            )
            .await?;
            flush(output).await?;
        }

        if let EncoderOutput::Owned(output) = self.output.archive_mut() {
            flush(output).await?;
        }

        self.finished = true;

        Ok(())
    }

    pub async fn finish(&mut self) -> io::Result<()> {
        let tail_bytes = self.finish_goodbye_table().await?;
        let mut state = self
            .state
            .pop()
            .ok_or_else(|| io_format_err!("encoder state stack underflow"))?;
        seq_write_pxar_entry(
            self.output.archive_mut().as_mut(),
            format::PXAR_GOODBYE,
            &tail_bytes,
            &mut state.write_position,
        )
        .await?;

        let end_offset = state.position();
        let payload_end_offset = state.payload_position();

        let encode_error = state.finish();
        if let Some(parent) = self.state.last_mut() {
            parent.write_position = end_offset;
            parent.payload_write_position = payload_end_offset;

            let file_offset = state
                .file_offset
                .expect("internal error: parent set but no file_offset?");

            parent.items.push(GoodbyeItem {
                hash: state.file_hash,
                offset: file_offset,
                size: end_offset - file_offset,
            });
            // propagate errors
            parent.merge_error(encode_error);
            Ok(())
        } else {
            match encode_error {
                Some(EncodeError::IncompleteFile) => io_bail!("incomplete file"),
                Some(EncodeError::IncompleteDirectory) => io_bail!("directory not finalized"),
                None => Ok(()),
            }
        }
    }

    async fn finish_goodbye_table(&mut self) -> io::Result<Vec<u8>> {
        let state = self.state_mut()?;
        let goodbye_offset = state.position();

        // "take" out the tail (to not leave an array of endian-swapped structs in `self`)
        let mut tail = take(&mut state.items);
        let tail_size = (tail.len() + 1) * size_of::<GoodbyeItem>();
        let goodbye_size = tail_size as u64 + size_of::<format::Header>() as u64;

        // sort, then create a BST
        tail.sort_unstable_by(|a, b| a.hash.cmp(&b.hash));

        let mut bst = Vec::with_capacity(tail.len() + 1);
        #[allow(clippy::uninit_vec)]
        unsafe {
            bst.set_len(tail.len());
        }
        binary_tree_array::copy(tail.len(), |src, dest| {
            let mut item = tail[src].clone();
            // fixup the goodbye table offsets to be relative and with the right endianess
            item.offset = goodbye_offset - item.offset;
            unsafe {
                std::ptr::write(&mut bst[dest], item.to_le());
            }
        });
        drop(tail);

        bst.push(
            GoodbyeItem {
                hash: format::PXAR_GOODBYE_TAIL_MARKER,
                offset: goodbye_offset - state.entry_offset,
                size: goodbye_size,
            }
            .to_le(),
        );

        // turn this into a byte vector since after endian-swapping we can no longer guarantee that
        // the items make sense:
        let data = bst.as_mut_ptr() as *mut u8;
        let capacity = bst.capacity() * size_of::<GoodbyeItem>();
        forget(bst);
        Ok(unsafe { Vec::from_raw_parts(data, tail_size, capacity) })
    }
}

/// Writer for a file object in a directory.
pub(crate) struct FileImpl<'a, S: SeqWrite> {
    /// Optional write redirection of file payloads to this sequential stream
    output: PxarVariant<&'a mut S, &'a mut S>,

    /// This file's `GoodbyeItem`. FIXME: We currently don't touch this, can we just push it
    /// directly instead of on Drop of FileImpl?
    goodbye_item: GoodbyeItem,

    /// While writing data to this file, this is how much space we still have left, this must reach
    /// exactly zero.
    remaining_size: u64,

    /// The directory stack with the last item being the directory containing this file. This is
    /// where we propagate the `IncompleteFile` error to, and where we insert our `GoodbyeItem`.
    parent: &'a mut EncoderState,
}

impl<'a, S: SeqWrite> Drop for FileImpl<'a, S> {
    fn drop(&mut self) {
        if self.remaining_size != 0 {
            self.parent.add_error(EncodeError::IncompleteFile);
        }

        self.parent.items.push(self.goodbye_item.clone());
    }
}

impl<'a, S: SeqWrite> FileImpl<'a, S> {
    /// Get the file offset to be able to reference it with `add_hardlink`.
    pub fn file_offset(&self) -> LinkOffset {
        LinkOffset(self.goodbye_item.offset)
    }

    fn check_remaining(&self, size: usize) -> io::Result<()> {
        if size as u64 > self.remaining_size {
            io_bail!("attempted to write more than previously allocated");
        } else {
            Ok(())
        }
    }

    /// Poll write interface to more easily connect to tokio/futures.
    #[cfg(feature = "tokio-io")]
    pub fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.check_remaining(data.len())?;
        let output = unsafe { Pin::new_unchecked(&mut *this.output.archive_mut()) };
        match output.poll_seq_write(cx, data) {
            Poll::Ready(Ok(put)) => {
                this.remaining_size -= put as u64;
                this.parent.write_position += put as u64;
                Poll::Ready(Ok(put))
            }
            other => other,
        }
    }

    /// Poll flush interface to more easily connect to tokio/futures.
    #[cfg(feature = "tokio-io")]
    pub fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        unsafe {
            self.map_unchecked_mut(|this| this.output.archive_mut())
                .poll_flush(cx)
        }
    }

    /// Poll close/shutdown interface to more easily connect to tokio/futures.
    ///
    /// This just calls flush, though, since we're just a virtual writer writing to the file
    /// provided by our encoder.
    #[cfg(feature = "tokio-io")]
    pub fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        unsafe {
            self.map_unchecked_mut(|this| this.output.archive_mut())
                .poll_flush(cx)
        }
    }

    /// Write file data for the current file entry in a pxar archive.
    ///
    /// This forwards to the output's `SeqWrite::poll_seq_write` and may write fewer bytes than
    /// requested. Check the return value for how many. There's also a `write_all` method available
    /// for convenience.
    pub async fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.check_remaining(data.len())?;
        let put = if let Some(mut output) = self.output.payload_mut() {
            let put =
                poll_fn(|cx| unsafe { Pin::new_unchecked(&mut output).poll_seq_write(cx, data) })
                    .await?;
            self.parent.payload_write_position += put as u64;
            put
        } else {
            let put = poll_fn(|cx| unsafe {
                Pin::new_unchecked(self.output.archive_mut()).poll_seq_write(cx, data)
            })
            .await?;
            self.parent.write_position += put as u64;
            put
        };

        self.remaining_size -= put as u64;
        Ok(put)
    }

    /// Completely write file data for the current file entry in a pxar archive.
    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.check_remaining(data.len())?;
        if let Some(ref mut output) = self.output.payload_mut() {
            seq_write_all(output, data, &mut self.parent.payload_write_position).await?;
        } else {
            seq_write_all(
                self.output.archive_mut(),
                data,
                &mut self.parent.write_position,
            )
            .await?;
        }
        self.remaining_size -= data.len() as u64;
        Ok(())
    }
}

#[cfg(feature = "tokio-io")]
impl<'a, S: SeqWrite> tokio::io::AsyncWrite for FileImpl<'a, S> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<io::Result<usize>> {
        FileImpl::poll_write(self, cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        FileImpl::poll_flush(self, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        FileImpl::poll_close(self, cx)
    }
}
