use std::any::Any;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Like Poll but Pending yields a value.
pub enum MaybeReady<T, F> {
    Ready(T),
    Pending(F),
}

/// Random access read implementation.
pub trait ReadAt {
    fn start_read_at<'a>(
        self: Pin<&'a Self>,
        cx: &mut Context,
        buf: &'a mut [u8],
        offset: u64,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>>;

    fn poll_complete<'a>(
        self: Pin<&'a Self>,
        op: ReadAtOperation<'a>,
    ) -> MaybeReady<io::Result<usize>, ReadAtOperation<'a>>;
}

pub struct ReadAtOperation<'a> {
    pub cookie: Box<dyn Any + Send + Sync>,
    _marker: PhantomData<&'a mut [u8]>,
}

impl<'a> ReadAtOperation<'a> {
    pub fn new<T: Into<Box<dyn Any + Send + Sync>>>(cookie: T) -> Self {
        Self {
            cookie: cookie.into(),
            _marker: PhantomData,
        }
    }
}

// awaitable helper:

pub trait ReadAtExt: ReadAt {
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
