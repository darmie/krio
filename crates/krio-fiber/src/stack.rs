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
pub(crate) struct Stack {
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
    pub(crate) fn new(requested_size: usize) -> Self {
        #[cfg(unix)]
        {
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

    /// Slice of the usable region. Shared mutable view — caller
    /// must respect aliasing rules.
    pub(crate) fn usable_slice_mut(&mut self) -> &mut [u8] {
        // SAFETY: `usable_start` and `usable_len` describe a region
        // we own (mmap or Box) for `Stack`'s lifetime.
        unsafe { std::slice::from_raw_parts_mut(self.usable_start, self.usable_len) }
    }
}

#[cfg(unix)]
unsafe fn mmap_stack(requested_size: usize) -> Stack {
    use libc::{
        MAP_ANONYMOUS, MAP_FAILED, MAP_PRIVATE, PROT_NONE, PROT_READ, PROT_WRITE, mmap, mprotect,
    };

    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    assert!(page_size > 0, "sysconf(_SC_PAGESIZE) must be positive");

    // Round usable size up to a page; add one extra page below for
    // the guard.
    let usable_size = requested_size.div_ceil(page_size) * page_size;
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
            // munmap the whole region, including the guard page.
            // Errors are ignored — there's nothing useful we can do
            // on drop, and the address came from mmap so it's valid.
            libc::munmap(self.base, self.total_bytes);
        }
        // On non-unix the Box<[u8]> drop handles itself.
    }
}

// `Stack` is automatically `!Send + !Sync` because of its raw pointer
// fields. Mirroring Fiber's single-thread invariant.
