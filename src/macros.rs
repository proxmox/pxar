/// Like failure's `format_err` but producing a `std::io::Error`.
macro_rules! io_format_err {
    ($($msg:tt)+) => {
        ::std::io::Error::new(::std::io::ErrorKind::Other, format!($($msg)+))
    };
}

/// Like failure's `bail` but producing a `std::io::Error`.
macro_rules! io_bail {
    ($($msg:tt)+) => {{
        return Err(io_format_err!($($msg)+));
    }};
}

/// Our dependency on `futures` is optional.
macro_rules! ready {
    ($expr:expr) => {{
        match $expr {
            std::task::Poll::Ready(r) => r,
            std::task::Poll::Pending => return std::task::Poll::Pending,
        }
    }};
}
