//! Asynchronous `pxar` random-access handling.
//!
//! Currently neither tokio nor futures have an `AsyncFileExt` variant.
//!
//! TODO: Implement a locking version for AsyncSeek+AsyncRead files?
