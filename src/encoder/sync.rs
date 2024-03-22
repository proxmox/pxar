//! Blocking `pxar` encoder.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::decoder::sync::StandardReader;
use crate::encoder::{self, LinkOffset, PayloadOffset, SeqWrite};
use crate::format;
use crate::util::poll_result_once;
use crate::{Metadata, PxarVariant};

/// Blocking `pxar` encoder.
///
/// This is the blocking I/O version of the `pxar` encoder. This will *not* work with an
/// asynchronous I/O object. I/O must always return `Poll::Ready`.
///
/// Attempting to use a `Waker` from this context *will* `panic!`
///
/// If you need to use asynchronous I/O, use `aio::Encoder`.
#[repr(transparent)]
pub struct Encoder<'a, T: SeqWrite + 'a> {
    inner: encoder::EncoderImpl<'a, T>,
}

impl<'a, T: io::Write + 'a> Encoder<'a, StandardWriter<T>> {
    /// Encode a `pxar` archive into a regular `std::io::Write` output.
    #[inline]
    pub fn from_std(output: T, metadata: &Metadata) -> io::Result<Encoder<'a, StandardWriter<T>>> {
        Encoder::new(
            PxarVariant::Unified(StandardWriter::new(output)),
            metadata,
            None,
        )
    }
}

impl<'a> Encoder<'a, StandardWriter<std::fs::File>> {
    /// Convenience shortcut for `File::create` followed by `Encoder::from_file`.
    pub fn create<'b, P: AsRef<Path>>(
        path: P,
        metadata: &'b Metadata,
    ) -> io::Result<Encoder<'a, StandardWriter<std::fs::File>>> {
        Encoder::new(
            PxarVariant::Unified(StandardWriter::new(std::fs::File::create(path.as_ref())?)),
            metadata,
            None,
        )
    }
}

impl<'a, T: SeqWrite + 'a> Encoder<'a, T> {
    /// Create a *blocking* encoder for an output implementing our internal write interface.
    ///
    /// Note that the `output`'s `SeqWrite` implementation must always return `Poll::Ready` and is
    /// not allowed to use the `Waker`, as this will cause a `panic!`.
    // Optionally attach a dedicated writer to redirect the payloads of regular files to a separate
    // output.
    pub fn new(
        output: PxarVariant<T, T>,
        metadata: &Metadata,
        prelude: Option<&[u8]>,
    ) -> io::Result<Self> {
        let output = output.wrap_multi(|output| output.into(), |payload_output| payload_output);

        Ok(Self {
            inner: poll_result_once(encoder::EncoderImpl::new(output, metadata, prelude))?,
        })
    }

    /// Create a new regular file in the archive. This returns a `File` object to which the
    /// contents have to be written out *completely*. Failing to do so will put the encoder into an
    /// error state.
    pub fn create_file<'b, P: AsRef<Path>>(
        &'b mut self,
        metadata: &Metadata,
        file_name: P,
        file_size: u64,
    ) -> io::Result<File<'b, T>>
    where
        'a: 'b,
    {
        Ok(File {
            inner: poll_result_once(self.inner.create_file(
                metadata,
                file_name.as_ref(),
                file_size,
            ))?,
        })
    }

    /// Convenience shortcut to add a *regular* file by path including its contents to the archive.
    pub fn add_file<P: AsRef<Path>>(
        &mut self,
        metadata: &Metadata,
        file_name: P,
        file_size: u64,
        content: &mut dyn io::Read,
    ) -> io::Result<LinkOffset> {
        poll_result_once(self.inner.add_file(
            metadata,
            file_name.as_ref(),
            file_size,
            &mut StandardReader::new(content),
        ))
    }

    /// Get current payload position for payload stream
    pub fn payload_position(&self) -> io::Result<PayloadOffset> {
        self.inner.payload_position()
    }

    /// Encode a payload reference pointing to given offset in the separate payload output
    ///
    /// Returns with error if the encoder instance has no separate payload output or encoding
    /// failed.
    pub async fn add_payload_ref(
        &mut self,
        metadata: &Metadata,
        file_name: &Path,
        file_size: u64,
        payload_offset: PayloadOffset,
    ) -> io::Result<LinkOffset> {
        poll_result_once(self.inner.add_payload_ref(
            metadata,
            file_name.as_ref(),
            file_size,
            payload_offset,
        ))
    }

    /// Add size to payload stream
    pub fn advance(&mut self, size: PayloadOffset) -> io::Result<()> {
        self.inner.advance(size)
    }

    /// Create a new subdirectory. Note that the subdirectory has to be finished by calling the
    /// `finish()` method, otherwise the entire archive will be in an error state.
    pub fn create_directory<P: AsRef<Path>>(
        &mut self,
        file_name: P,
        metadata: &Metadata,
    ) -> io::Result<()> {
        poll_result_once(self.inner.create_directory(file_name.as_ref(), metadata))
    }

    /// Finish this directory. This is mandatory, encodes the end for the current directory.
    pub fn finish(&mut self) -> io::Result<()> {
        poll_result_once(self.inner.finish())
    }

    /// Close the encoder instance. This is mandatory, encodes the end for the optional payload
    /// output stream, if some is given
    pub fn close(self) -> io::Result<()> {
        poll_result_once(self.inner.close())
    }

    /// Add a symbolic link to the archive.
    pub fn add_symlink<PF: AsRef<Path>, PT: AsRef<Path>>(
        &mut self,
        metadata: &Metadata,
        file_name: PF,
        target: PT,
    ) -> io::Result<()> {
        poll_result_once(
            self.inner
                .add_symlink(metadata, file_name.as_ref(), target.as_ref()),
        )
    }

    /// Add a hard link to the archive.
    pub fn add_hardlink<PF: AsRef<Path>, PT: AsRef<Path>>(
        &mut self,
        file_name: PF,
        target: PT,
        offset: LinkOffset,
    ) -> io::Result<()> {
        poll_result_once(
            self.inner
                .add_hardlink(file_name.as_ref(), target.as_ref(), offset),
        )
    }

    /// Add a device node to the archive.
    pub fn add_device<P: AsRef<Path>>(
        &mut self,
        metadata: &Metadata,
        file_name: P,
        device: format::Device,
    ) -> io::Result<()> {
        poll_result_once(self.inner.add_device(metadata, file_name.as_ref(), device))
    }

    /// Add a fifo node to the archive.
    pub fn add_fifo<P: AsRef<Path>>(
        &mut self,
        metadata: &Metadata,
        file_name: P,
    ) -> io::Result<()> {
        poll_result_once(self.inner.add_fifo(metadata, file_name.as_ref()))
    }

    /// Add a socket node to the archive.
    pub fn add_socket<P: AsRef<Path>>(
        &mut self,
        metadata: &Metadata,
        file_name: P,
    ) -> io::Result<()> {
        poll_result_once(self.inner.add_socket(metadata, file_name.as_ref()))
    }
}

/// This is a "file" inside a pxar archive, to which the initially declared amount of data should
/// be written.
///
/// Writing more or less than the designated amount is an error and will cause the produced archive
/// to be broken.
#[repr(transparent)]
pub struct File<'a, S: SeqWrite> {
    inner: encoder::FileImpl<'a, S>,
}

impl<'a, S: SeqWrite> File<'a, S> {
    /// Get the file offset to be able to reference it with `add_hardlink`.
    pub fn file_offset(&self) -> LinkOffset {
        self.inner.file_offset()
    }
}

impl<'a, S: SeqWrite> io::Write for File<'a, S> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        poll_result_once(self.inner.write(data))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Pxar encoder write adapter for [`Write`](std::io::Write).
pub struct StandardWriter<T> {
    inner: Option<T>,
}

impl<T: io::Write> StandardWriter<T> {
    /// Make a new [`SeqWrite`] wrapper for an object implementing [`Write`](std::io::Write).
    pub fn new(inner: T) -> Self {
        Self { inner: Some(inner) }
    }

    fn inner(&mut self) -> io::Result<&mut T> {
        self.inner
            .as_mut()
            .ok_or_else(|| io_format_err!("write after close"))
    }

    fn pin_to_inner(self: Pin<&mut Self>) -> io::Result<&mut T> {
        unsafe { self.get_unchecked_mut() }.inner()
    }
}

impl<T: io::Write> SeqWrite for StandardWriter<T> {
    fn poll_seq_write(
        self: Pin<&mut Self>,
        _cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        Poll::Ready(this.inner()?.write(buf))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<()>> {
        Poll::Ready(self.pin_to_inner().and_then(|inner| inner.flush()))
    }
}
