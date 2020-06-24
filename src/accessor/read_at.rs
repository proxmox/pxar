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
        ReadAtImpl::New(self, buf, offset)
    }
}

impl<T: ReadAt> ReadAtExt for T {}

pub enum ReadAtImpl<'a, T: ReadAt> {
    Invalid,
    New(&'a T, &'a mut [u8], u64),
    Pending(&'a T, ReadAtOperation<'a>),
    Ready(io::Result<usize>),
}

impl<T: ReadAt> ReadAtImpl<'_, T> {
    fn take(&mut self) -> Self {
        std::mem::replace(self, ReadAtImpl::Invalid)
    }
}

impl<'a, T: ReadAt> Future for ReadAtImpl<'a, T> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = unsafe { Pin::into_inner_unchecked(self) };
        loop {
            match match this.take() {
                ReadAtImpl::New(reader, buf, offset) => {
                    let pin = unsafe { Pin::new_unchecked(reader) };
                    (pin.start_read_at(cx, buf, offset), reader)
                }
                ReadAtImpl::Pending(reader, op) => {
                    let pin = unsafe { Pin::new_unchecked(reader) };
                    (pin.poll_complete(op), reader)
                }
                ReadAtImpl::Ready(out) => return Poll::Ready(out),
                ReadAtImpl::Invalid => panic!("poll after ready"),
            } {
                (MaybeReady::Ready(out), _reader) => return Poll::Ready(out),
                (MaybeReady::Pending(op), reader) => *this = ReadAtImpl::Pending(reader, op),
            }
        }
    }
}
