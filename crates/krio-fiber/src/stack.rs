//! Fiber stack allocation.
//!
//! Two strategies, picked at compile time:
//!
//! - **Unix (`cfg(unix)`)**: `mmap` a region of `(stack_size + page_size)`
//!   bytes, then `mprotect(PROT_NONE)` the lowest page to act as a
//!   guard page. A stack overflow that touches that page traps with
//!   SIGSEGV instead of silently corrupting unrelated heap data.
//!
//! - **Other targets**: heap-allocated `Box<[u8]>`. No guard page —
//!   stack overflow corrupts the heap. Acceptable for testing on
//!   Windows / WASM until a target-specific allocator lands.
//!
//! The `Stack` type is the same in both cases; only the constructor
//! differs.

/// One fiber's stack. The `usable` slice is what the fiber actually
/// runs on. On Unix we also own a separate guard page below it.
///
/// Public for [`raw`](crate::raw) users that manage their own contexts:
/// pass [`Stack::usable_slice_mut`] to [`raw::Context::new`](crate::raw::Context::new)
/// and keep the `Stack` alive as long as the context can run.
pub struct Stack {
    #[cfg(unix)]
    base: *mut libc::c_void,
    #[cfg(unix)]
    total_bytes: usize,
    /// Heap fallback for non-unix targets. Unused on unix.
    #[cfg(not(unix))]
    _heap: Box<[u8]>,
    /// Pointer to the start of the *usable* region (above the guard
    /// page on unix, the start of the boxed slice elsewhere).
    usable_start: *mut u8,
    /// Length of the usable region in bytes.
    usable_len: usize,
}

impl Stack {
    /// Allocate a new fiber stack with at least `requested_size`
    /// usable bytes. Sizes are rounded up to a page boundary on
    /// unix, or to 16 bytes on other targets.
    pub fn new(requested_size: usize) -> Self {
        #[cfg(unix)]
        {
            if let Some(stack) = cache::take(usable_size(requested_size)) {
                return stack;
            }
            unsafe { mmap_stack(requested_size) }
        }
        #[cfg(not(unix))]
        {
            heap_stack(requested_size)
        }
    }

    /// Pointer to the lowest byte of the usable region.
    #[allow(dead_code)] // available for callers that want raw access
    pub(crate) fn usable_start(&self) -> *mut u8 {
        self.usable_start
    }

    /// Length of the usable region in bytes.
    pub fn usable_len(&self) -> usize {
        self.usable_len
    }

    /// Slice of the usable region. Shared mutable view — caller
    /// must respect aliasing rules.
    pub fn usable_slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: `usable_start` and `usable_len` describe a region
        // we own (mmap or Box) for `Stack`'s lifetime.
        unsafe { std::slice::from_raw_parts_mut(self.usable_start, self.usable_len) }
    }
}

#[cfg(unix)]
fn page_size() -> usize {
    static PAGE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *PAGE.get_or_init(|| {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        assert!(page_size > 0, "sysconf(_SC_PAGESIZE) must be positive");
        page_size
    })
}

/// The usable bytes a stack of `requested_size` gets: a whole number
/// of pages.
#[cfg(unix)]
fn usable_size(requested_size: usize) -> usize {
    requested_size.div_ceil(page_size()) * page_size()
}

#[cfg(unix)]
unsafe fn mmap_stack(requested_size: usize) -> Stack {
    use libc::{
        MAP_ANONYMOUS, MAP_FAILED, MAP_PRIVATE, PROT_NONE, PROT_READ, PROT_WRITE, mmap, mprotect,
    };

    let page_size = page_size();

    // Round usable size up to a page; add one extra page below for
    // the guard.
    let usable_size = usable_size(requested_size);
    let total = usable_size + page_size;

    // mmap an anonymous region. PROT_READ|PROT_WRITE for the whole
    // range — we'll mprotect the guard page next.
    let base = unsafe {
        mmap(
            std::ptr::null_mut(),
            total,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert!(
        base != MAP_FAILED,
        "krio-fiber: mmap failed for fiber stack"
    );

    // The guard page is the *bottom* page. The fiber's stack grows
    // downward from the top; an overflow trips through the guard.
    let rc = unsafe { mprotect(base, page_size, PROT_NONE) };
    assert!(
        rc == 0,
        "krio-fiber: mprotect(PROT_NONE) failed on guard page"
    );

    // Usable region starts above the guard.
    let usable_start = unsafe { (base as *mut u8).add(page_size) };

    Stack {
        base,
        total_bytes: total,
        usable_start,
        usable_len: usable_size,
    }
}

#[cfg(not(unix))]
fn heap_stack(requested_size: usize) -> Stack {
    let aligned = requested_size.next_multiple_of(16).max(16);
    let mut boxed = vec![0u8; aligned].into_boxed_slice();
    let usable_start = boxed.as_mut_ptr();
    let usable_len = boxed.len();
    Stack {
        _heap: boxed,
        usable_start,
        usable_len,
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            if cache::park(self) {
                return;
            }
            // munmap the whole region, including the guard page.
            // Errors are ignored — there's nothing useful we can do
            // on drop, and the address came from mmap so it's valid.
            libc::munmap(self.base, self.total_bytes);
        }
        // On non-unix the Box<[u8]> drop handles itself.
    }
}

/// Stacks a thread keeps for the next fiber of the same size, so a
/// fiber that finishes and the next one to start cost no `mmap`,
/// `mprotect` or `munmap`. A kept stack still holds the pages its
/// last fiber touched, and its contents are stale: a fiber writes
/// before it reads, and a collector scans only from a suspended
/// fiber's saved stack pointer.
#[cfg(unix)]
mod cache {
    use super::Stack;
    use std::cell::RefCell;

    /// Stacks one thread keeps.
    const MAX_PARKED: usize = 16;

    struct Parked {
        base: *mut libc::c_void,
        total_bytes: usize,
        usable_start: *mut u8,
        usable_len: usize,
    }

    struct Pool(Vec<Parked>);

    impl Drop for Pool {
        fn drop(&mut self) {
            for p in self.0.drain(..) {
                // SAFETY: `base` and `total_bytes` came from `mmap`, and
                // nothing else owns the mapping.
                unsafe { libc::munmap(p.base, p.total_bytes) };
            }
        }
    }

    thread_local! {
        static POOL: RefCell<Pool> = const { RefCell::new(Pool(Vec::new())) };
    }

    /// A kept stack with exactly `usable_len` usable bytes, the most
    /// recently kept first.
    pub(super) fn take(usable_len: usize) -> Option<Stack> {
        POOL.try_with(|pool| {
            let mut pool = pool.borrow_mut();
            let i = pool.0.iter().rposition(|p| p.usable_len == usable_len)?;
            let p = pool.0.swap_remove(i);
            Some(Stack {
                base: p.base,
                total_bytes: p.total_bytes,
                usable_start: p.usable_start,
                usable_len: p.usable_len,
            })
        })
        .ok()
        .flatten()
    }

    /// Keep `stack`'s mapping for a later fiber; false when this thread
    /// already keeps its share, or is gone.
    pub(super) fn park(stack: &Stack) -> bool {
        POOL.try_with(|pool| {
            let mut pool = pool.borrow_mut();
            if pool.0.len() >= MAX_PARKED {
                return false;
            }
            pool.0.push(Parked {
                base: stack.base,
                total_bytes: stack.total_bytes,
                usable_start: stack.usable_start,
                usable_len: stack.usable_len,
            });
            true
        })
        .unwrap_or(false)
    }

    #[cfg(test)]
    pub(super) fn parked() -> usize {
        POOL.with(|pool| pool.borrow().0.len())
    }

    #[cfg(test)]
    pub(super) const LIMIT: usize = MAX_PARKED;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_stack_is_reused_for_the_next_of_its_size() {
        let first = Stack::new(64 << 10);
        let start = first.usable_start() as usize;
        drop(first);
        let second = Stack::new(64 << 10);
        assert_eq!(second.usable_start() as usize, start);
    }

    #[test]
    fn a_stack_of_another_size_is_not_reused() {
        let small = Stack::new(64 << 10);
        let start = small.usable_start() as usize;
        drop(small);
        let large = Stack::new(128 << 10);
        assert_ne!(large.usable_start() as usize, start);
        assert_eq!(large.usable_len(), 128 << 10);
        assert_eq!(cache::parked(), 1);
    }

    #[test]
    fn a_thread_keeps_a_bounded_number_of_stacks() {
        let stacks: Vec<Stack> = (0..cache::LIMIT + 8)
            .map(|_| Stack::new(64 << 10))
            .collect();
        drop(stacks);
        assert_eq!(cache::parked(), cache::LIMIT);
    }

    #[test]
    fn a_reused_stack_is_usable_to_its_top() {
        let mut first = Stack::new(64 << 10);
        first.usable_slice_mut().fill(0xAB);
        drop(first);
        let mut second = Stack::new(64 << 10);
        let len = second.usable_len();
        let slice = second.usable_slice_mut();
        slice[0] = 1;
        slice[len - 1] = 2;
        assert_eq!((slice[0], slice[len - 1]), (1, 2));
    }
}

// `Stack` is automatically `!Send + !Sync` because of its raw pointer
// fields. Mirroring Fiber's single-thread invariant.
