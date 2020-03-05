//! The `pxar` encoder state machine.
//!
//! This is the implementation used by both the synchronous and async pxar wrappers.

use std::io;
use std::mem::{forget, size_of, size_of_val, take};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use endian_trait::Endian;

use crate::binary_tree_array;
use crate::decoder::SeqRead;
use crate::format::{self, GoodbyeItem};
use crate::poll_fn::poll_fn;
use crate::Metadata;

pub mod sync;

#[doc(inline)]
pub use sync::Encoder;

/// Sequential write interface used by the encoder's state machine.
///
/// This is our internal writer trait which is available for `std::io::Write` types in the
/// synchronous wrapper and for both `tokio` and `future` `AsyncWrite` types in the asynchronous
/// wrapper.
pub trait SeqWrite {
    fn poll_seq_write(
        self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>>;

    /// While writing to a pxar archive we need to remember how much dat we've written to track some
    /// offsets. Particularly items like the goodbye table need to be able to compute offsets to
    /// further back in the archive.
    fn poll_position(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<u64>>;

    /// To avoid recursively borrowing each time we nest into a subdirectory we add this helper.
    /// Otherwise starting a subdirectory will get a trait object pointing to `T`, nesting another
    /// subdirectory in that would have a trait object pointing to the trait object, and so on.
    fn as_trait_object(&mut self) -> &mut dyn SeqWrite
    where
        Self: Sized,
    {
        self as &mut dyn SeqWrite
    }
}

/// Allow using trait objects for generics taking a `SeqWrite`.
impl<'a> SeqWrite for &mut (dyn SeqWrite + 'a) {
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

    fn poll_position(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<u64>> {
        unsafe { self.map_unchecked_mut(|this| &mut **this).poll_position(cx) }
    }

    fn as_trait_object(&mut self) -> &mut dyn SeqWrite
    where
        Self: Sized,
    {
        &mut **self
    }
}

trait PxarStruct: Endian {
    const HTYPE: u64;
}

impl PxarStruct for format::Entry {
    const HTYPE: u64 = format::PXAR_ENTRY;
}

impl PxarStruct for format::QuotaProjectId {
    const HTYPE: u64 = format::PXAR_QUOTA_PROJID;
}

impl<'a> dyn SeqWrite + 'a {
    /// awaitable version of `poll_position`.
    async fn position(&mut self) -> io::Result<u64> {
        poll_fn(|cx| unsafe { Pin::new_unchecked(&mut *self).poll_position(cx) }).await
    }

    /// awaitable verison of `poll_seq_write`.
    async fn seq_write(&mut self, buf: &[u8]) -> io::Result<usize> {
        poll_fn(|cx| unsafe { Pin::new_unchecked(&mut *self).poll_seq_write(cx, buf) }).await
    }

    /// Write the entire contents of a buffer, handling short writes.
    async fn seq_write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let got = self.seq_write(buf).await?;
            buf = &buf[got..];
        }
        Ok(())
    }

    /// Write an endian-swappable struct.
    async fn seq_write_struct<E: Endian>(&mut self, data: E) -> io::Result<()> {
        let data = data.to_le();
        self.seq_write_all(unsafe {
            std::slice::from_raw_parts(&data as *const E as *const u8, size_of_val(&data))
        })
        .await
    }

    /// Write a pxar entry.
    async fn seq_write_pxar_entry(&mut self, htype: u64, data: &[u8]) -> io::Result<()> {
        self.seq_write_struct(format::Header::with_content_size(htype, data.len() as u64))
            .await?;
        self.seq_write_all(data).await
    }

    /// Write a pxar entry terminated by an additional zero which is not contained in the provided
    /// data buffer.
    async fn seq_write_pxar_entry_zero(&mut self, htype: u64, data: &[u8]) -> io::Result<()> {
        self.seq_write_struct(format::Header::with_content_size(
            htype,
            1 + data.len() as u64,
        ))
        .await?;
        self.seq_write_all(data).await?;
        self.seq_write_all(&[0u8]).await
    }

    /// Write a pxar entry consiting of an endian-swappable struct.
    async fn seq_write_pxar_struct_entry<E: Endian>(
        &mut self,
        htype: u64,
        data: E,
    ) -> io::Result<()> {
        let data = data.to_le();
        self.seq_write_pxar_entry(htype, unsafe {
            std::slice::from_raw_parts(&data as *const E as *const u8, size_of_val(&data))
        })
        .await
    }

    /// Write a pxar entry consiting of an endian-swappable struct.
    async fn seq_write_pxar_struct<E: PxarStruct>(&mut self, data: E) -> io::Result<()> {
        self.seq_write_pxar_struct_entry(E::HTYPE, data).await
    }
}

#[derive(Default)]
struct EncoderState {
    /// Goodbye items for this directory, excluding the tail.
    items: Vec<GoodbyeItem>,

    /// Error condition: the user dropped a file without writing all of its contents.
    incomplete_file_error: bool,

    /// Error condition: the user dropped the directory without finalizing it.
    incomplete_directory_error: bool,

    /// Offset of this directory's ENTRY.
    entry_offset: u64,

    /// Offset to this directory's first FILENAME.
    files_offset: u64,

    /// If this is a subdirectory, this points to the this directory's FILENAME.
    file_offset: Option<u64>,

    /// If this is a subdirectory, this contains this directory's hash for the goodbye item.
    file_hash: u64,
}

/// The encoder state machine implementation for a directory.
///
/// We use `async fn` to implement the encoder state machine so that we can easily plug in both
/// synchronous or `async` I/O objects in as output.
pub(crate) struct EncoderImpl<'a, T: SeqWrite + 'a> {
    output: T,
    state: EncoderState,
    parent: Option<&'a mut EncoderState>,
    finished: bool,
}

impl<'a, T: SeqWrite + 'a> Drop for EncoderImpl<'a, T> {
    fn drop(&mut self) {
        if let Some(ref mut parent) = self.parent {
            // propagate errors:
            if self.state.incomplete_file_error {
                parent.incomplete_file_error = true;
            }

            if self.state.incomplete_directory_error || !self.finished {
                parent.incomplete_directory_error = true;
            }
        } else if !self.finished {
            // FIXME: how do we deal with this?
            eprintln!("Encoder dropped without finishing!");
        }
    }
}

impl<'a, T: SeqWrite + 'a> EncoderImpl<'a, T> {
    pub async fn new(output: T, metadata: &Metadata) -> io::Result<EncoderImpl<'a, T>> {
        if !metadata.is_dir() {
            io_bail!("directory metadata must contain the directory mode flag");
        }
        let mut this = Self {
            output,
            state: EncoderState::default(),
            parent: None,
            finished: false,
        };

        this.encode_metadata(metadata).await?;
        this.state.files_offset = (&mut this.output as &mut dyn SeqWrite).position().await?;

        Ok(this)
    }

    fn check(&self) -> io::Result<()> {
        if self.state.incomplete_file_error {
            io_bail!("incomplete file");
        } else {
            Ok(())
        }
    }

    pub async fn create_file<'b>(
        &'b mut self,
        metadata: &Metadata,
        file_name: &Path,
        file_size: u64,
    ) -> io::Result<FileImpl<'b>>
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
    ) -> io::Result<FileImpl<'b>>
    where
        'a: 'b,
    {
        self.check()?;

        let file_offset = (&mut self.output as &mut dyn SeqWrite).position().await?;
        self.start_file_do(metadata, file_name).await?;

        (&mut self.output as &mut dyn SeqWrite)
            .seq_write_struct(format::Header::with_content_size(
                format::PXAR_PAYLOAD,
                file_size,
            ))
            .await?;

        let payload_data_offset = (&mut self.output as &mut dyn SeqWrite).position().await?;

        let meta_size = payload_data_offset - file_offset;

        Ok(FileImpl {
            output: &mut self.output,
            goodbye_item: GoodbyeItem {
                hash: format::hash_filename(file_name),
                offset: file_offset,
                size: file_size + meta_size,
            },
            remaining_size: file_size,
            parent: &mut self.state,
        })
    }

    pub async fn add_file(
        &mut self,
        metadata: &Metadata,
        file_name: &Path,
        file_size: u64,
        content: &mut dyn SeqRead,
    ) -> io::Result<()> {
        let mut file = self.create_file(metadata, file_name, file_size).await?;
        let mut buf = crate::util::vec_new(4096);
        loop {
            let got = content.seq_read(&mut buf).await?;
            if got == 0 {
                break;
            } else {
                file.write_all(&buf[..got]).await?;
            }
        }
        Ok(())
    }

    /// Helper
    #[inline]
    async fn position(&mut self) -> io::Result<u64> {
        (&mut self.output as &mut dyn SeqWrite).position().await
    }

    pub async fn create_directory<'b>(
        &'b mut self,
        file_name: &Path,
        metadata: &Metadata,
    ) -> io::Result<EncoderImpl<'b, &'b mut dyn SeqWrite>>
    where
        'a: 'b,
    {
        self.check()?;

        if !metadata.is_dir() {
            io_bail!("directory metadata must contain the directory mode flag");
        }

        let file_name = file_name.as_os_str().as_bytes();
        let file_hash = format::hash_filename(file_name);

        let file_offset = self.position().await?;
        self.encode_filename(file_name).await?;

        let entry_offset = self.position().await?;
        self.encode_metadata(&metadata).await?;

        let files_offset = self.position().await?;

        Ok(EncoderImpl {
            output: self.output.as_trait_object(),
            state: EncoderState {
                entry_offset,
                files_offset,
                file_offset: Some(file_offset),
                file_hash: file_hash,
                ..Default::default()
            },
            parent: Some(&mut self.state),
            finished: false,
        })
    }

    async fn start_file_do(&mut self, metadata: &Metadata, file_name: &[u8]) -> io::Result<()> {
        self.encode_filename(file_name).await?;
        self.encode_metadata(&metadata).await?;
        Ok(())
    }

    async fn encode_metadata(&mut self, metadata: &Metadata) -> io::Result<()> {
        (&mut self.output as &mut dyn SeqWrite)
            .seq_write_pxar_struct(metadata.stat.clone())
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
        (&mut self.output as &mut dyn SeqWrite)
            .seq_write_pxar_entry(format::PXAR_XATTR, &xattr.data)
            .await
    }

    async fn write_acls(&mut self, acl: &crate::Acl) -> io::Result<()> {
        for acl in &acl.users {
            (&mut self.output as &mut dyn SeqWrite)
                .seq_write_pxar_struct_entry(format::PXAR_ACL_USER, acl.clone())
                .await?;
        }

        for acl in &acl.groups {
            (&mut self.output as &mut dyn SeqWrite)
                .seq_write_pxar_struct_entry(format::PXAR_ACL_GROUP, acl.clone())
                .await?;
        }

        if let Some(acl) = &acl.group_obj {
            (&mut self.output as &mut dyn SeqWrite)
                .seq_write_pxar_struct_entry(format::PXAR_ACL_GROUP_OBJ, acl.clone())
                .await?;
        }

        if let Some(acl) = &acl.default {
            (&mut self.output as &mut dyn SeqWrite)
                .seq_write_pxar_struct_entry(format::PXAR_ACL_DEFAULT, acl.clone())
                .await?;
        }

        for acl in &acl.default_users {
            (&mut self.output as &mut dyn SeqWrite)
                .seq_write_pxar_struct_entry(format::PXAR_ACL_DEFAULT_USER, acl.clone())
                .await?;
        }

        for acl in &acl.default_groups {
            (&mut self.output as &mut dyn SeqWrite)
                .seq_write_pxar_struct_entry(format::PXAR_ACL_DEFAULT_GROUP, acl.clone())
                .await?;
        }

        Ok(())
    }

    async fn write_file_capabilities(&mut self, fcaps: &format::FCaps) -> io::Result<()> {
        (&mut self.output as &mut dyn SeqWrite)
            .seq_write_pxar_entry(format::PXAR_FCAPS, &fcaps.data)
            .await
    }

    async fn write_quota_project_id(
        &mut self,
        quota_project_id: &format::QuotaProjectId,
    ) -> io::Result<()> {
        (&mut self.output as &mut dyn SeqWrite)
            .seq_write_pxar_struct(quota_project_id.clone())
            .await
    }

    async fn encode_filename(&mut self, file_name: &[u8]) -> io::Result<()> {
        (&mut self.output as &mut dyn SeqWrite)
            .seq_write_pxar_entry_zero(format::PXAR_FILENAME, file_name)
            .await
    }

    pub async fn finish(mut self) -> io::Result<()> {
        let tail_bytes = self.finish_goodbye_table().await?;
        (&mut self.output as &mut dyn SeqWrite)
            .seq_write_pxar_entry(format::PXAR_GOODBYE, &tail_bytes)
            .await?;
        if let Some(parent) = &mut self.parent {
            let file_offset = self
                .state
                .file_offset
                .expect("internal error: parent set but no file_offset?");

            let end_offset = (&mut self.output as &mut dyn SeqWrite).position().await?;

            parent.items.push(GoodbyeItem {
                hash: self.state.file_hash,
                offset: file_offset,
                size: end_offset - file_offset,
            });
        }
        self.finished = true;
        Ok(())
    }

    async fn finish_goodbye_table(&mut self) -> io::Result<Vec<u8>> {
        let goodbye_offset = (&mut self.output as &mut dyn SeqWrite).position().await?;

        // "take" out the tail (to not leave an array of endian-swapped structs in `self`)
        let mut tail = take(&mut self.state.items);
        let tail_size = (tail.len() + 1) * size_of::<GoodbyeItem>();
        let goodbye_size = tail_size as u64 + size_of::<format::Header>() as u64;

        // sort, then create a BST
        tail.sort_unstable_by(|a, b| a.hash.cmp(&b.hash));

        let mut bst = Vec::with_capacity(tail.len() + 1);
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
                offset: goodbye_offset - self.state.entry_offset,
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
pub struct FileImpl<'a> {
    output: &'a mut dyn SeqWrite,

    /// This file's `GoodbyeItem`. FIXME: We currently don't touch this, can we just push it
    /// directly instead of on Drop of FileImpl?
    goodbye_item: GoodbyeItem,

    /// While writing data to this file, this is how much space we still have left, this must reach
    /// exactly zero.
    remaining_size: u64,

    /// The directory containing this file. This is where we propagate the `incomplete_file_error`
    /// to, and where we insert our `GoodbyeItem`.
    parent: &'a mut EncoderState,
}

impl<'a> Drop for FileImpl<'a> {
    fn drop(&mut self) {
        if self.remaining_size != 0 {
            self.parent.incomplete_file_error = true;
        }

        self.parent.items.push(self.goodbye_item.clone());
    }
}

impl<'a> FileImpl<'a> {
    fn check_remaining(&self, size: usize) -> io::Result<()> {
        if size as u64 > self.remaining_size {
            io_bail!("attempted to write more than previously allocated");
        } else {
            Ok(())
        }
    }

    /// Write file data for the current file entry in a pxar archive.
    ///
    /// This forwards to the output's `SeqWrite::poll_seq_write` and may write fewer bytes than
    /// requested. Check the return value for how many. There's also a `write_all` method available
    /// for convenience.
    pub async fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.check_remaining(data.len())?;
        let put = self.output.seq_write(data).await?;
        self.remaining_size -= put as u64;
        Ok(put)
    }

    /// Completely write file data for the current file entry in a pxar archive.
    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.check_remaining(data.len())?;
        self.output.seq_write_all(data).await?;
        self.remaining_size -= data.len() as u64;
        Ok(())
    }
}
