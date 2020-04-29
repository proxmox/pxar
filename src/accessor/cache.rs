//! Cache helper for random access pxar accesses.

use std::sync::Arc;

/// A simple asynchronous caching trait to help speed up random access for pxar archives.
pub trait Cache<K, V: ?Sized> {
    /// Fetch a value from this cache store.
    fn fetch(&self, key: K) -> Option<Arc<V>>;

    /// Update the cache with a value.
    ///
    /// It is up to the implementation to choose whether or not to store the value. Note that since
    /// all values are `Arc`s, the values will remain alive as long as they're in use even if
    /// they're not cached.
    fn insert(&self, key: K, value: Arc<V>);
}
