//! [`ZeroOnCancel`] — plaintext-on-cancel zeroize wrapper (I-FUSE-4).
//!
//! Extends the existing `Zeroizing<Vec<u8>>` discipline used by
//! `mem_gateway::DecryptCache` (commit `de5a239`) to handler-future
//! cancellation paths. When a tokio task driving an async FUSE
//! handler is dropped before completion, every in-flight
//! `ZeroOnCancel<Vec<u8>>` it owns gets `zeroize`'d on drop —
//! regardless of which `await` point was cancelled.

use std::ops::{Deref, DerefMut};
use zeroize::Zeroize;

/// Smart pointer that calls [`Zeroize::zeroize`] on drop.
///
/// Use this for plaintext buffers that flow through async FUSE
/// handlers. The unconditional zeroize on drop means a cancelled
/// future cannot leave plaintext in heap memory:
///
/// ```ignore
/// async fn read(&self, ino: u64, off: u64, size: u32) -> Result<Vec<u8>, FuseError> {
///     let plaintext: ZeroOnCancel<Vec<u8>> = self.gateway.read(ino, off, size).await?.into();
///     // ... if this future is cancelled, `plaintext` drops -> bytes are zeroized.
///     Ok(plaintext.into_inner())
/// }
/// ```
///
/// Conceptually equivalent to `zeroize::Zeroizing<T>`; named
/// distinctly because the kiseki-fuse contract explicitly tags
/// cancellation as the threat model.
#[derive(Debug, Default)]
pub struct ZeroOnCancel<T: Zeroize> {
    inner: T,
}

impl<T: Zeroize> ZeroOnCancel<T> {
    /// Wrap a value so it is zeroized on drop.
    pub const fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Unwrap without zeroizing — caller takes responsibility for the
    /// inner value's lifecycle from here.
    ///
    /// Use this when handing off to a finalize path that consumes the
    /// bytes (e.g., copying into a libfuse reply buffer; libfuse
    /// internalizes the bytes and we don't need to zeroize after).
    pub fn into_inner(self) -> T {
        // ManuallyDrop suppresses our Drop; ptr::read moves the
        // field out by value. Avoids requiring `T: Default` (which
        // `mem::take` would have demanded).
        let me = std::mem::ManuallyDrop::new(self);
        // SAFETY: `me.inner` is a valid, non-shared `T`; reading it
        // by pointer moves it. `me` is never accessed after this
        // (ManuallyDrop suppresses its destructor).
        unsafe { std::ptr::read(std::ptr::addr_of!(me.inner)) }
    }
}

impl<T: Zeroize> From<T> for ZeroOnCancel<T> {
    fn from(inner: T) -> Self {
        Self::new(inner)
    }
}

impl<T: Zeroize> Deref for ZeroOnCancel<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T: Zeroize> DerefMut for ZeroOnCancel<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T: Zeroize> Drop for ZeroOnCancel<T> {
    fn drop(&mut self) {
        self.inner.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// Test type whose `Zeroize` impl counts invocations — proves
    /// the wrapper called zeroize without depending on post-drop
    /// allocator state (which is undefined and racy).
    struct CountingZ {
        zeroized: &'static AtomicUsize,
    }

    impl Zeroize for CountingZ {
        fn zeroize(&mut self) {
            self.zeroized.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn drop_calls_inner_zeroize() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let zoc = ZeroOnCancel::new(CountingZ { zeroized: &COUNT });
        assert_eq!(COUNT.load(Ordering::SeqCst), 0);
        drop(zoc);
        assert_eq!(COUNT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn into_inner_does_not_zeroize() {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let zoc = ZeroOnCancel::new(CountingZ { zeroized: &COUNT });
        let _inner = zoc.into_inner();
        // Inner moved out; our Drop did NOT run.
        assert_eq!(COUNT.load(Ordering::SeqCst), 0);
        // Caller now owns `_inner`; when it goes out of scope its
        // own Zeroize is NOT invoked (CountingZ has no Drop impl
        // beyond the trait method, which only fires on explicit
        // `.zeroize()` calls).
    }
}
