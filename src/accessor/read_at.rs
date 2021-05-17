//! Async `ReadAt` trait.

use std::any::Any;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Like Poll but Pending yields a value.
pub enum MaybeReady<T, F> {
    /// Same as [`Poll::Ready`].
    Ready(T),

    /// Same as [`Poll::Pending`], but contains a "cookie" identifying the ongoing operation.
    /// Without this value, it is impossible to make further progress on the operation.
    Pending(F),
}

/// Random access read implementation.
pub trait ReadAt {
    /// Begin a read operation.
    ///
    /// Contrary to tokio and future's `AsyncRead` traits, this implements positional reads and
    /// therefore allows multiple operations to run simultaneously. In order to accomplish this,
    /// the result of this call includes a [`ReadAtOperation`] "cookie" identifying the particular
    /// read operation. This is necessary, since with an async runtime multiple such calls can come
    /// from the same thread and even the same task.
    ///
    /// It is possible that this operation succeeds immediately, in which case
    /// `MaybeRead::Ready(Ok(bytes))` is returned containing the number of bytes read.
    ///
    /// If the operation takes longer to complete, returns `MaybeReady::Pending(cookie)`, and the
    /// current taks will be notified via `cx.waker()` when progress can be made. Once that
    /// happens, [`poll_complete`](ReadAt::poll_complete) should be called using the returned
    /// `cookie`.
    ///
    /// On error, returns `MaybeRead::Ready(Err(err))`.
    fn start_read_at<'a>(
        self: Pin<&'a Self>,
        cx: &mut Context,
        buf: &'a mut [u8],
        offset: u64,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>>;

    /// Attempt to complete a previously started read operation identified by the provided
    /// [`ReadAtOperation`].
    ///
    /// If the read operation is finished, returns `MaybeReady::Ready(Ok(bytes))` containing the
    /// number of bytes read.
    ///
    /// If the operation is not yet completed, returns `MaybeReady::Pending(cookie)`, returning the
    /// (possibly modified) operation cookie again to be reused for the next call to
    /// `poll_complete`.
    ///
    /// On error, returns `MaybeRead::Ready(Err(err))`.
    fn poll_complete<'a>(
        self: Pin<&'a Self>,
        op: ReadAtOperation<'a>,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>>;
}

/// A "cookie" identifying a particular [`ReadAt`] operation.
pub struct ReadAtOperation<'a> {
    /// The implementor of the [`ReadAt`] trait is responsible for what type of data is contained
    /// in here.
    ///
    /// Note that the contained data needs to implement `Drop` so that dropping the "cookie"
    /// cancels the operation correctly.
    ///
    /// Apart from this field, the struct only contains phantom data.
    pub cookie: Box<dyn Any + Send + Sync>,
    _marker: PhantomData<&'a mut [u8]>,
}

impl<'a> ReadAtOperation<'a> {
    /// Create a new [`ReadAtOperation`].
    pub fn new<T: Into<Box<dyn Any + Send + Sync>>>(cookie: T) -> Self {
        Self {
            cookie: cookie.into(),
            _marker: PhantomData,
        }
    }
}

// awaitable helper:

/// [`ReadAt`] extension trait, akin to `AsyncReadExt`.
pub trait ReadAtExt: ReadAt {
    /// Equivalent to `async fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>`.
    fn read_at<'a>(&'a self, buf: &'a mut [u8], offset: u64) -> ReadAtImpl<'a, Self>
    where
        Self: Sized,
    {
        ReadAtImpl::new(self, buf, offset)
    }
}

impl<T: ReadAt> ReadAtExt for T {}

/// Future returned by [`ReadAtExt::read_at`](ReadAtExt::read_at()).
#[repr(transparent)]
pub struct ReadAtImpl<'a, T: ReadAt> {
    state: ReadAtState<'a, T>,
}

impl<'a, T: ReadAt> ReadAtImpl<'a, T> {
    fn new(read: &'a T, buf: &'a mut [u8], offset: u64) -> Self {
        Self {
            state: ReadAtState::New(read, buf, offset),
        }
    }
}

impl<'a, T: ReadAt> Future for ReadAtImpl<'a, T> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        unsafe { self.map_unchecked_mut(|this| &mut this.state).poll(cx) }
    }
}

enum ReadAtState<'a, T: ReadAt> {
    Invalid,
    New(&'a T, &'a mut [u8], u64),
    Pending(&'a T, ReadAtOperation<'a>),
}

impl<T: ReadAt> ReadAtState<'_, T> {
    fn take(&mut self) -> Self {
        std::mem::replace(self, ReadAtState::Invalid)
    }
}

impl<'a, T: ReadAt> Future for ReadAtState<'a, T> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = unsafe { Pin::into_inner_unchecked(self) };
        loop {
            match match this.take() {
                ReadAtState::New(reader, buf, offset) => {
                    let pin = unsafe { Pin::new_unchecked(reader) };
                    (pin.start_read_at(cx, buf, offset), reader)
                }
                ReadAtState::Pending(reader, op) => {
                    let pin = unsafe { Pin::new_unchecked(reader) };
                    (pin.poll_complete(op), reader)
                }
                ReadAtState::Invalid => panic!("poll after ready"),
            } {
                (MaybeReady::Ready(out), _reader) => return Poll::Ready(out),
                (MaybeReady::Pending(op), reader) => *this = ReadAtState::Pending(reader, op),
            }
        }
    }
}
