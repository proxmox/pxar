//! `poll_fn` reimplementation as it is otherwise the only thing we need from the futures crate.
//!
//! Our `futures` crate dependency is optional.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct PollFn<F> {
    func: Option<F>,
}

pub fn poll_fn<F, R>(func: F) -> PollFn<F>
where
    F: FnMut(&mut Context) -> Poll<R>,
{
    PollFn { func: Some(func) }
}

impl<F, R> Future for PollFn<F>
where
    F: FnMut(&mut Context) -> Poll<R>,
{
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let this = unsafe { self.get_unchecked_mut() };
        match &mut this.func {
            None => panic!("poll() after Ready"),
            Some(func) => {
                let res = func(cx);
                if res.is_ready() {
                    this.func = None;
                }
                res
            }
        }
    }
}
