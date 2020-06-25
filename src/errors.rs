use std::error::Error;
use std::fmt;

#[derive(Clone, Debug)]
pub enum TimeError {
    Underflow(std::time::SystemTimeError),
    Overflow(std::time::Duration),
}

impl fmt::Display for TimeError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            TimeError::Underflow(st) => st.fmt(f),
            TimeError::Overflow(dur) => write!(f, "timestamp out of range: {:?}", dur),
        }
    }
}

impl Error for TimeError {}

impl From<std::time::SystemTimeError> for TimeError {
    fn from(t: std::time::SystemTimeError) -> Self {
        TimeError::Underflow(t)
    }
}
