//! Blocking `pxar` encoder.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::decoder::sync::StandardReader;
use crate::encoder::{self, SeqWrite};
use crate::util::poll_result_once;
use crate::Metadata;

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
    pub fn from_std(output: T, metadata: &Metadata) -> io::Result<Encoder<StandardWriter<T>>> {
        Encoder::new(StandardWriter::new(output), metadata)
    }
}

impl<'a> Encoder<'a, StandardWriter<std::fs::File>> {
    /// Convenience shortcut for `File::create` followed by `Encoder::from_file`.
    pub fn create<'b, P: AsRef<Path>>(
        path: P,
        metadata: &'b Metadata,
    ) -> io::Result<Encoder<'a, StandardWriter<std::fs::File>>> {
        Encoder::new(
            StandardWriter::new(std::fs::File::create(path.as_ref())?),
            metadata,
        )
    }
}

impl<'a, T: SeqWrite + 'a> Encoder<'a, T> {
    /// Create a *blocking* encoder from an output implementing our internal write interface.
    ///
    /// Note that the `output`'s `SeqWrite` implementation must always return `Poll::Ready` and is
    /// not allowed to use the `Waker`, as this will cause a `panic!`.
    pub fn new(output: T, metadata: &Metadata) -> io::Result<Self> {
        Ok(Self {
            inner: poll_result_once(encoder::EncoderImpl::new(output, metadata))?,
        })
    }

    /// Create a new regular file to the archive. This returns a `File` object to which the
    /// contents have to be written out completely. Failing to do so will put the encoder into an
    /// error state.
    pub fn create_file<P: AsRef<Path>>(
        &'a mut self,
        metadata: &Metadata,
        file_name: P,
        file_size: u64,
    ) -> io::Result<File<'a>> {
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
    ) -> io::Result<()> {
        poll_result_once(self.inner.add_file(
            metadata,
            file_name.as_ref(),
            file_size,
            &mut StandardReader::new(content),
        ))
    }

    /// Create a new subdirectory. Note that the subdirectory has to be finished by calling the
    /// `finish()` method, otherwise the entire archive will be in an error state.
    pub fn create_directory<P: AsRef<Path>>(
        &'a mut self,
        file_name: P,
        metadata: &Metadata,
    ) -> io::Result<Encoder<'a, &'a mut dyn SeqWrite>> {
        Ok(Encoder {
            inner: poll_result_once(self.inner.create_directory(file_name.as_ref(), metadata))?,
        })
    }

    /// Finish this directory. This is mandatory, otherwise the `Drop` handler will `panic!`.
    pub fn finish(self) -> io::Result<()> {
        poll_result_once(self.inner.finish())
    }
}

#[repr(transparent)]
pub struct File<'a> {
    inner: encoder::FileImpl<'a>,
}

impl<'a> io::Write for File<'a> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        poll_result_once(self.inner.write(data))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Pxar encoder write adapter for `std::io::Write`.
pub struct StandardWriter<T> {
    inner: T,
    position: u64,
}

impl<T: io::Write> StandardWriter<T> {
    pub fn new(inner: T) -> Self {
        Self { inner, position: 0 }
    }
}

impl<T: io::Write> SeqWrite for StandardWriter<T> {
    fn poll_seq_write(
        self: Pin<&mut Self>,
        _cx: &mut Context,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = unsafe { self.get_unchecked_mut() };
        Poll::Ready(match this.inner.write(buf) {
            Ok(got) => {
                this.position += got as u64;
                Ok(got)
            }
            Err(err) => Err(err),
        })
    }

    fn poll_position(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.as_ref().position))
    }
}
