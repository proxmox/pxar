//! Asynchronous `pxar` format handling.

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::encoder::{self, LinkOffset, SeqWrite};
use crate::format;
use crate::Metadata;

// #[cfg(feature = "futures-io")]
// use crate::decoder::aio::FuturesReader;
// #[cfg(feature = "tokio-io")]
// use crate::decoder::aio::TokioReader;

/// Asynchronous `pxar` encoder.
///
/// This is the `async` version of the `pxar` encoder.
#[repr(transparent)]
pub struct Encoder<'a, T: SeqWrite + 'a> {
    inner: encoder::EncoderImpl<'a, T>,
}

#[cfg(feature = "futures-io")]
impl<'a, T: futures::io::AsyncWrite + 'a> Encoder<'a, FuturesWriter<T>> {
    /// Encode a `pxar` archive into a `futures::io::AsyncWrite` output.
    #[inline]
    pub async fn from_futures(
        output: T,
        metadata: &Metadata,
    ) -> io::Result<Encoder<'a, FuturesWriter<T>>> {
        Encoder::new(FuturesWriter::new(output), metadata).await
    }
}

#[cfg(feature = "tokio-io")]
impl<'a, T: tokio::io::AsyncWrite + 'a> Encoder<'a, TokioWriter<T>> {
    /// Encode a `pxar` archive into a `tokio::io::AsyncWrite` output.
    #[inline]
    pub async fn from_futures(
        output: T,
        metadata: &Metadata,
    ) -> io::Result<Encoder<'a, TokioWriter<T>>> {
        Encoder::new(TokioWriter::new(output), metadata).await
    }
}

#[cfg(feature = "tokio-fs")]
impl<'a> Encoder<'a, TokioWriter<tokio::fs::File>> {
    /// Convenience shortcut for `File::create` followed by `Encoder::from_tokio`.
    pub async fn create<'b, P: AsRef<Path>>(
        path: P,
        metadata: &'b Metadata,
    ) -> io::Result<Encoder<'a, TokioWriter<tokio::fs::File>>> {
        Encoder::new(
            TokioWriter::new(tokio::fs::File::create(path.as_ref()).await?),
            metadata,
        )
        .await
    }
}

impl<'a, T: SeqWrite + 'a> Encoder<'a, T> {
    /// Create an asynchronous encoder for an output implementing our internal write interface.
    pub async fn new(output: T, metadata: &Metadata) -> io::Result<Encoder<'a, T>> {
        Ok(Self {
            inner: encoder::EncoderImpl::new(output, metadata).await?,
        })
    }

    /// Create a new regular file in the archive. This returns a `File` object to which the
    /// contents have to be written out *completely*. Failing to do so will put the encoder into an
    /// error state.
    pub async fn create_file<'b, P: AsRef<Path>>(
        &'b mut self,
        metadata: &Metadata,
        file_name: P,
        file_size: u64,
    ) -> io::Result<File<'b>>
    where
        'a: 'b,
    {
        Ok(File {
            inner: self
                .inner
                .create_file(metadata, file_name.as_ref(), file_size)
                .await?,
        })
    }

    // /// Convenience shortcut to add a *regular* file by path including its contents to the archive.
    // pub async fn add_file<P, F>(
    //     &mut self,
    //     metadata: &Metadata,
    //     file_name: P,
    //     file_size: u64,
    //     content: &mut dyn tokio::io::Read,
    // ) -> io::Result<()>
    // where
    //     P: AsRef<Path>,
    //     F: AsAsyncReader,
    // {
    //     self.inner.add_file(
    //         metadata,
    //         file_name.as_ref(),
    //         file_size,
    //         content.as_async_reader(),
    //     ).await
    // }

    /// Create a new subdirectory. Note that the subdirectory has to be finished by calling the
    /// `finish()` method, otherwise the entire archive will be in an error state.
    pub async fn create_directory<'b, P: AsRef<Path>>(
        &'b mut self,
        file_name: P,
        metadata: &Metadata,
    ) -> io::Result<Encoder<'b, &'b mut dyn SeqWrite>>
    where
        'a: 'b,
    {
        Ok(Encoder {
            inner: self
                .inner
                .create_directory(file_name.as_ref(), metadata)
                .await?,
        })
    }

    /// Finish this directory. This is mandatory, otherwise the `Drop` handler will `panic!`.
    pub async fn finish(self) -> io::Result<T> {
        self.inner.finish().await
    }

    /// Cancel this directory and get back the contained writer.
    pub fn into_writer(self) -> T {
        self.inner.into_writer()
    }

    /// Add a symbolic link to the archive.
    pub async fn add_symlink<PF: AsRef<Path>, PT: AsRef<Path>>(
        &mut self,
        metadata: &Metadata,
        file_name: PF,
        target: PT,
    ) -> io::Result<()> {
        self.inner
            .add_symlink(metadata, file_name.as_ref(), target.as_ref())
            .await
    }

    /// Add a hard link to the archive.
    pub async fn add_hardlink<PF: AsRef<Path>, PT: AsRef<Path>>(
        &mut self,
        file_name: PF,
        target: PT,
        offset: LinkOffset,
    ) -> io::Result<()> {
        self.inner
            .add_hardlink(file_name.as_ref(), target.as_ref(), offset)
            .await
    }

    /// Add a device node to the archive.
    pub async fn add_device<P: AsRef<Path>>(
        &mut self,
        metadata: &Metadata,
        file_name: P,
        device: format::Device,
    ) -> io::Result<()> {
        self.inner
            .add_device(metadata, file_name.as_ref(), device)
            .await
    }

    /// Add a device node to the archive.
    pub async fn add_fifo<P: AsRef<Path>>(
        &mut self,
        metadata: &Metadata,
        file_name: P,
    ) -> io::Result<()> {
        self.inner.add_fifo(metadata, file_name.as_ref()).await
    }

    /// Add a device node to the archive.
    pub async fn add_socket<P: AsRef<Path>>(
        &mut self,
        metadata: &Metadata,
        file_name: P,
    ) -> io::Result<()> {
        self.inner.add_socket(metadata, file_name.as_ref()).await
    }
}

#[repr(transparent)]
pub struct File<'a> {
    inner: encoder::FileImpl<'a>,
}

impl<'a> File<'a> {
    /// Get the file offset to be able to reference it with `add_hardlink`.
    pub fn file_offset(&self) -> LinkOffset {
        self.inner.file_offset()
    }

    /// Write file data for the current file entry in a pxar archive.
    pub async fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.inner.write(data).await
    }

    /// Completely write file data for the current file entry in a pxar archive.
    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.inner.write_all(data).await
    }
}

#[cfg(feature = "futures-io")]
impl<'a> futures::io::AsyncWrite for File<'a> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, data: &[u8]) -> Poll<io::Result<usize>> {
        unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.poll_write(cx, data)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.poll_close(cx)
    }
}

#[cfg(feature = "tokio-io")]
impl<'a> tokio::io::AsyncWrite for File<'a> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context, data: &[u8]) -> Poll<io::Result<usize>> {
        unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.poll_write(cx, data)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
        unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.poll_close(cx)
    }
}

/// Pxar encoder write adapter for `futures::io::AsyncWrite`.
#[cfg(feature = "futures-io")]
mod futures_writer {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use crate::encoder::SeqWrite;

    pub struct FuturesWriter<T> {
        inner: Option<T>,
    }

    impl<T: futures::io::AsyncWrite> FuturesWriter<T> {
        pub fn new(inner: T) -> Self {
            Self { inner: Some(inner) }
        }

        fn inner_mut(&mut self) -> io::Result<Pin<&mut T>> {
            let inner = self
                .inner
                .as_mut()
                .ok_or_else(|| io_format_err!("write after close"))?;
            Ok(unsafe { Pin::new_unchecked(inner) })
        }

        fn inner(self: Pin<&mut Self>) -> io::Result<Pin<&mut T>> {
            unsafe { self.get_unchecked_mut() }.inner_mut()
        }
    }

    impl<T: futures::io::AsyncWrite> SeqWrite for FuturesWriter<T> {
        fn poll_seq_write(
            self: Pin<&mut Self>,
            cx: &mut Context,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = unsafe { self.get_unchecked_mut() };
            this.inner_mut()?.poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
            self.inner()?.poll_flush(cx)
        }
    }
}

pub use futures_writer::FuturesWriter;

/// Pxar encoder write adapter for `tokio::io::AsyncWrite`.
#[cfg(feature = "tokio-io")]
mod tokio_writer {
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use crate::encoder::SeqWrite;

    pub struct TokioWriter<T> {
        inner: Option<T>,
    }

    impl<T: tokio::io::AsyncWrite> TokioWriter<T> {
        pub fn new(inner: T) -> Self {
            Self { inner: Some(inner) }
        }

        fn inner_mut(&mut self) -> io::Result<Pin<&mut T>> {
            let inner = self
                .inner
                .as_mut()
                .ok_or_else(|| io_format_err!("write after close"))?;
            Ok(unsafe { Pin::new_unchecked(inner) })
        }

        fn inner(self: Pin<&mut Self>) -> io::Result<Pin<&mut T>> {
            unsafe { self.get_unchecked_mut() }.inner_mut()
        }
    }

    impl<T: tokio::io::AsyncWrite> SeqWrite for TokioWriter<T> {
        fn poll_seq_write(
            self: Pin<&mut Self>,
            cx: &mut Context,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = unsafe { self.get_unchecked_mut() };
            this.inner_mut()?.poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<io::Result<()>> {
            self.inner()?.poll_flush(cx)
        }
    }
}

pub use tokio_writer::TokioWriter;
