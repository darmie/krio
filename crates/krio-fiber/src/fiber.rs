//! Fiber type + the global "currently active fiber" hook that
//! [`yield_now`] uses to swap back to the caller.

use std::any::Any;
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use krio_core::{Suspension, Task};

use crate::arch::{SAVED_FP_OFFSET, SAVED_FRAME_BYTES, SAVED_RET_OFFSET, krio_fiber_switch};
use crate::stack::Stack;

/// Lifecycle state of a fiber.
///
/// Visible to the host between `resume` calls. The host doesn't see
/// `Running` because `resume` is synchronous — when control returns
/// to the host, the fiber has either yielded (Suspended) or
/// terminated (Done / Errored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiberState {
    /// Created but never resumed.
    New,
    /// Yielded back to the caller; can be resumed again.
    Suspended,
    /// Closure returned normally. Resuming again panics.
    Done,
    /// Closure panicked. The payload is on the fiber until taken via
    /// [`Fiber::take_error`]. Resuming again panics.
    Errored,
}

/// Result of a single [`Fiber::resume`] call. Mirrors [`FiberState`]
/// minus `New` (you can't observe `New` from a `resume` return — by
/// the time `resume` returns, the fiber has run at least to its
/// first yield or end).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiberStep {
    /// Fiber yielded; another `resume` will continue it.
    Yielded,
    /// Fiber's entry closure returned. Further `resume` calls panic.
    Done,
    /// Fiber's entry closure panicked. The panic payload is parked
    /// on the fiber; retrieve it with [`Fiber::take_error`]. Further
    /// `resume` calls panic.
    Errored,
}

/// A stackful coroutine. Owns its stack; runs an `FnOnce` closure
/// that may call [`yield_now`] from any depth to suspend back to the
/// caller of [`Fiber::resume`].
///
/// Single-threaded — fibers are explicitly `!Send`. A multi-threaded
/// variant could lift this restriction once the scheduling story is
/// concrete.
pub struct Fiber {
    /// Fiber's stack. Owns its memory (mmap region with a guard page
    /// on Unix, heap `Box<[u8]>` elsewhere). Freed on drop. The
    /// fiber grows the stack downward from the top of the usable
    /// region.
    _stack: Stack,
    /// Current saved stack pointer for the fiber. On `resume`, we
    /// load from here; on `yield_now`, we save into here.
    fiber_sp: *mut u8,
    /// Saved stack pointer for the caller (where `resume` was
    /// invoked from). Populated on each `resume` call.
    caller_sp: *mut u8,
    /// Set to `true` by the trampoline when the user closure returns
    /// or panics. Combined with `errored` it determines [`FiberState`].
    done: bool,
    /// Set to `true` by the trampoline when the user closure panicked.
    /// `done == true && errored == true` → [`FiberState::Errored`].
    errored: bool,
    /// Has [`Fiber::resume`] been called at least once?
    started: bool,
    /// Panic payload captured by the trampoline. Taken via
    /// [`Fiber::take_error`].
    error_payload: Option<Box<dyn Any + Send + 'static>>,
    /// Cooperative cancellation flag. Set by [`Fiber::cancel`];
    /// observed via [`Fiber::is_cancelled`] (and [`is_cancelled`]
    /// from inside the fiber). Single-thread fibers don't need an
    /// atomic — `Cell<bool>` is enough — but we use a `Cell` here
    /// so external code can flip it through `&self`.
    cancelled: Cell<bool>,
    /// Optional absolute deadline in milliseconds since the Unix
    /// epoch. Polled by [`Fiber::is_deadline_passed`] /
    /// [`is_deadline_passed`].
    deadline_ms: Option<f64>,
    /// Value handed in by the host on `resume_with(v)`. The fiber
    /// reads it from inside via [`yield_value`]'s return.
    input_slot: Cell<Option<Box<dyn Any + 'static>>>,
    /// Value handed out by the fiber's `yield_value(v)`. The host
    /// reads it via [`Fiber::take_yield_value`] after resume.
    output_slot: Cell<Option<Box<dyn Any + 'static>>>,
    /// `u64` fast-path slots. The generic `<I>` / `<O>` API above
    /// always allocates a fresh `Box<dyn Any>` per resume/yield;
    /// hosts whose only payload is a fixed-size scalar (e.g. a
    /// NaN-boxed VM value) pay one `Box::new` + one drop per
    /// fiber switch, which dominates allocator traffic in a tight
    /// yield loop. These slots skip the box entirely. Filled via
    /// [`Fiber::resume_with_u64`] / [`yield_u64`]; read via
    /// [`Fiber::take_yield_u64`] / [`take_input_u64`]. The generic
    /// slots are left in place so heterogeneous payloads still
    /// work — callers pick the API matching their payload type.
    input_u64: Cell<Option<u64>>,
    /// See [`Fiber::input_u64`].
    output_u64: Cell<Option<u64>>,
    /// Process-unique id. Stable across resumes; useful for tagging
    /// fibers in logs and scheduler queues.
    id: u64,
    /// Optional human-readable label. Free-form; no semantic meaning.
    name: Option<String>,
    /// Heap-pinned trampoline state — kept alive while the fiber is
    /// running so the trampoline's pointer-to-closure stays valid.
    /// Boxed because the asm reads it through a stable address.
    _trampoline_state: Box<TrampolineState>,
    /// Mark `!Send` and `!Sync` explicitly. The fiber's stack is
    /// owned, but its saved state contains pointers into the host
    /// thread; moving across threads would corrupt them.
    _not_send: std::marker::PhantomData<*mut ()>,
}

/// Pointed at by the trampoline; carries the closure + a back-pointer
/// to the fiber's `done` flag and `caller_sp` for `yield_now`.
struct TrampolineState {
    // Consumed by `fiber_run`, which only exists on targets with a native
    // context switch; unused (but still allocated) on unsupported targets
    // (see `crate::arch` for which those are — note aarch64-windows is
    // deliberately among them).
    #[cfg_attr(
        not(any(target_arch = "x86_64", all(target_arch = "aarch64", not(windows)))),
        allow(dead_code)
    )]
    closure: Option<Box<dyn FnOnce()>>,
    /// Pointer to the parent fiber's `done` flag. Set after the
    /// closure returns or panics.
    done_flag: *mut bool,
    /// Pointer to the parent fiber's `errored` flag. Set when the
    /// closure panicked.
    errored_flag: *mut bool,
    /// Pointer to the parent fiber's `error_payload` slot. The
    /// trampoline writes the captured panic payload here.
    error_payload_slot: *mut Option<Box<dyn Any + Send + 'static>>,
    /// Pointer to the parent fiber's `caller_sp` slot. Used by
    /// `yield_now` to know which sp to switch back to.
    caller_sp_slot: *mut *mut u8,
    /// Pointer to the parent fiber's `fiber_sp` slot. `yield_now`
    /// saves the current sp here.
    fiber_sp_slot: *mut *mut u8,
}

thread_local! {
    /// `*mut TrampolineState` for the currently running fiber, or
    /// null when running on the host stack. `yield_now` reads this
    /// to know how to switch back to the caller.
    static ACTIVE_TRAMPOLINE: Cell<*mut TrampolineState> =
        const { Cell::new(std::ptr::null_mut()) };

    /// `*mut Fiber` for the currently running fiber, or null when on
    /// the host stack. Used by the fiber-side accessors
    /// ([`is_cancelled`], [`is_deadline_passed`]) so user code can
    /// poll cancellation without having to thread the `Fiber` handle
    /// through every call.
    static ACTIVE_FIBER: Cell<*mut Fiber> =
        const { Cell::new(std::ptr::null_mut()) };
}

/// Default fiber stack size — 64 KB is comfortable for nested calls
/// while still cheap to allocate. Tune via [`Fiber::with_stack_size`]
/// if you know your workload.
pub const DEFAULT_STACK_SIZE: usize = 64 * 1024;

/// Process-wide id counter. Atomic so multi-threaded callers (whose
/// fibers run on separate threads) still get unique ids.
static NEXT_FIBER_ID: AtomicU64 = AtomicU64::new(1);

fn next_fiber_id() -> u64 {
    NEXT_FIBER_ID.fetch_add(1, Ordering::Relaxed)
}

impl Fiber {
    /// Create a fiber that runs `f` to completion, with the default
    /// stack size. The fiber does not start running until
    /// [`Fiber::resume`] is called.
    pub fn new<F>(f: F) -> Self
    where
        F: FnOnce() + 'static,
    {
        Self::with_stack_size(DEFAULT_STACK_SIZE, f)
    }

    /// Create a fiber with a specific stack size (in bytes). Rounded
    /// up to a 16-byte boundary for ABI compliance on aarch64 + x86_64.
    pub fn with_stack_size<F>(stack_size: usize, f: F) -> Self
    where
        F: FnOnce() + 'static,
    {
        let aligned = stack_size.max(SAVED_FRAME_BYTES + 64).next_multiple_of(16);
        let mut stack = Stack::new(aligned);

        let mut state = Box::new(TrampolineState {
            closure: Some(Box::new(f) as Box<dyn FnOnce()>),
            done_flag: std::ptr::null_mut(),
            errored_flag: std::ptr::null_mut(),
            error_payload_slot: std::ptr::null_mut(),
            caller_sp_slot: std::ptr::null_mut(),
            fiber_sp_slot: std::ptr::null_mut(),
        });

        let fiber_sp =
            unsafe { prepare_initial_stack(stack.usable_slice_mut(), &mut *state as *mut _) };

        Fiber {
            _stack: stack,
            fiber_sp,
            caller_sp: std::ptr::null_mut(),
            done: false,
            errored: false,
            started: false,
            error_payload: None,
            cancelled: Cell::new(false),
            deadline_ms: None,
            input_slot: Cell::new(None),
            output_slot: Cell::new(None),
            input_u64: Cell::new(None),
            output_u64: Cell::new(None),
            id: next_fiber_id(),
            name: None,
            _trampoline_state: state,
            _not_send: std::marker::PhantomData,
        }
    }

    /// Set an absolute deadline (milliseconds since the Unix epoch).
    /// User code inside the fiber can poll
    /// [`Fiber::is_deadline_passed`] / [`is_deadline_passed`] and
    /// yield/return early if the deadline has been crossed.
    /// Replaces any previous deadline.
    pub fn set_deadline_ms(&mut self, deadline_ms: f64) {
        self.deadline_ms = Some(deadline_ms);
    }

    /// Clear the deadline.
    pub fn clear_deadline(&mut self) {
        self.deadline_ms = None;
    }

    /// Read the current deadline, if any.
    pub fn deadline_ms(&self) -> Option<f64> {
        self.deadline_ms
    }

    /// Mark this fiber as cancelled. The flag is only observed on
    /// the next call to [`is_cancelled`] / [`Fiber::is_cancelled`]
    /// — krio-fiber does *not* preempt; it's the fiber's job to
    /// poll and bail out cooperatively.
    pub fn cancel(&self) {
        self.cancelled.set(true);
    }

    /// Whether [`Fiber::cancel`] has been called.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.get()
    }

    /// Whether the deadline (if any) has been reached.
    pub fn is_deadline_passed(&self) -> bool {
        match self.deadline_ms {
            Some(d) => current_time_ms() >= d,
            None => false,
        }
    }

    /// Resume the fiber, passing `input` into it. The fiber receives
    /// the value as the return of its [`yield_value`] call (or via
    /// [`take_input`] for the first call).
    ///
    /// On the way back, [`take_yield_value`] retrieves whatever the
    /// fiber yielded.
    pub fn resume_with<I: 'static>(&mut self, input: I) -> FiberStep {
        self.input_slot
            .set(Some(Box::new(input) as Box<dyn Any + 'static>));
        self.resume()
    }

    /// `u64` fast path for [`Fiber::resume_with`]. Skips the
    /// `Box<dyn Any>` allocation in `resume_with::<u64>`. Use
    /// in tight yield/resume loops where the payload is a fixed
    /// scalar (e.g. a NaN-boxed VM value).
    pub fn resume_with_u64(&mut self, input: u64) -> FiberStep {
        self.input_u64.set(Some(input));
        self.resume()
    }

    /// Take the most recent yield as `u64`, paired with [`yield_u64`].
    /// Returns `None` if the fiber didn't yield via the `u64` fast
    /// path (e.g. it used the generic [`yield_value`]); the generic
    /// `output_slot` is left untouched so the caller can still call
    /// [`Fiber::take_yield_any`] for the boxed value.
    pub fn take_yield_u64(&self) -> Option<u64> {
        self.output_u64.take()
    }

    /// Take the value the fiber most recently yielded. Returns
    /// `Some(v)` if the fiber yielded a `T` and the value hasn't
    /// already been taken; `None` if no value is buffered, or if
    /// the buffered value isn't a `T` — in which case the value is
    /// left in place so a subsequent `take_yield_value::<U>()` (or
    /// [`Fiber::take_yield_any`]) can claim it. This makes
    /// fall-through dispatch on multiple yield-value types
    /// ergonomic without losing the value on the first miss.
    pub fn take_yield_value<O: 'static>(&self) -> Option<O> {
        let boxed = self.output_slot.take()?;
        match boxed.downcast::<O>() {
            Ok(b) => Some(*b),
            Err(original) => {
                // Wrong type — restore the slot so the caller can
                // try a different `O` next time. Without this,
                // the value would silently disappear on a type
                // mismatch.
                self.output_slot.set(Some(original));
                None
            }
        }
    }

    /// Take the most recent yield value as an opaque `Box<dyn Any>`,
    /// leaving the slot empty. Use when the caller wants to drive
    /// its own downcast logic (e.g. matching across many possible
    /// yield types) or when the host needs to forward the value
    /// without inspecting it.
    pub fn take_yield_any(&self) -> Option<Box<dyn Any + 'static>> {
        self.output_slot.take()
    }

    /// Whether the fiber has a yielded value waiting to be taken.
    /// Cheap; doesn't consume.
    pub fn has_yield_value(&self) -> bool {
        // `Cell::take()` would consume; we use a transient
        // take/set pair to peek without changing observable state.
        let v = self.output_slot.take();
        let present = v.is_some();
        self.output_slot.set(v);
        present
    }

    /// Lifecycle state of this fiber. See [`FiberState`].
    pub fn state(&self) -> FiberState {
        match (self.started, self.done, self.errored) {
            (false, _, _) => FiberState::New,
            (true, false, _) => FiberState::Suspended,
            (true, true, false) => FiberState::Done,
            (true, true, true) => FiberState::Errored,
        }
    }

    /// Whether the fiber has reached a terminal state (Done or
    /// Errored). Equivalent to `matches!(state, Done | Errored)`.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Take the panic payload from a fiber that finished in
    /// [`FiberState::Errored`]. Returns `None` if the fiber didn't
    /// error or the payload has already been taken.
    pub fn take_error(&mut self) -> Option<Box<dyn Any + Send + 'static>> {
        self.error_payload.take()
    }

    /// Process-unique id assigned at construction. Stable across
    /// resumes; useful as a key in scheduler queues / log lines.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Optional human-readable label set via [`Fiber::set_name`].
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Attach (or replace) a debugging label.
    pub fn set_name<N: Into<String>>(&mut self, name: N) {
        self.name = Some(name.into());
    }

    /// Saved stack pointer for a suspended fiber. Points at the top
    /// of the saved-callee-registers region produced by
    /// [`crate::arch::krio_fiber_switch`].
    ///
    /// Only meaningful while the fiber is in [`FiberState::Suspended`] —
    /// for a `New` fiber this points at a synthetic startup frame; for
    /// `Done` / `Errored` fibers the stack is logically dead. Hosts
    /// that need to scan fiber stacks (e.g. for GC root tracing) should
    /// check [`Fiber::state`] first.
    ///
    /// Returns `null` if the fiber's stack hasn't been initialised yet
    /// (cannot happen in practice — Fiber construction always runs
    /// `prepare_initial_stack`).
    pub fn saved_sp(&self) -> *const u8 {
        self.fiber_sp
    }

    /// The fiber's usable stack region as `(lowest_addr, len)` —
    /// excludes the guard page. For conservative GC scanning, the
    /// live window of a suspended fiber is
    /// `[saved_sp, lowest_addr + len)`: the context switch spills all
    /// callee-saved registers (including d8-d15) onto the stack at
    /// `saved_sp` before switching away, so a word-by-word scan of
    /// that window covers register-resident pointers too.
    ///
    /// The region is owned by this `Fiber` and stays valid (and
    /// address-stable) until the `Fiber` is dropped — unregister it
    /// from any scanner before dropping.
    pub fn stack_range(&self) -> (*const u8, usize) {
        (
            self._stack.usable_start() as *const u8,
            self._stack.usable_len(),
        )
    }

    /// The HOST-side stack pointer saved when this fiber was last
    /// resumed, or null if the fiber is not currently running. While
    /// the fiber runs, the host stack is suspended at this SP — a
    /// conservative scanner walking the host stack should scan
    /// `[caller_sp, host_stack_top)` instead of probing a live SP
    /// that now points into the fiber's stack.
    pub fn caller_sp(&self) -> *const u8 {
        self.caller_sp
    }

    /// Read the saved frame-pointer register (`rbp` on x86_64, `x29`
    /// on aarch64) from a suspended fiber's saved-registers region.
    /// This is the entry point a frame-chain walker uses to traverse
    /// the fiber's stack frames — `saved_fp` points at the fiber's
    /// deepest active frame; each frame's first word is its own
    /// caller's saved fp, forming the standard SysV/AAPCS64 chain.
    ///
    /// Returns `None` if the fiber is in [`FiberState::New`] (no
    /// meaningful saved state yet — the synthetic startup frame
    /// contains zeros) or has terminated.
    ///
    /// # Build requirements
    /// The returned pointer is only meaningful when the build
    /// preserves frame pointers. Rust on Linux defaults to omitting
    /// them; in that configuration `saved_fp` returns whatever the
    /// `rbp` / `x29` register happened to hold at the suspension
    /// point (typically `null`). Force them on workspace-wide with
    /// `RUSTFLAGS=-Cforce-frame-pointers=yes` or via
    /// `.cargo/config.toml`. macOS toolchains force frame pointers
    /// by convention, so dev builds there work without the flag.
    ///
    /// # Safety
    /// Always safe to call. The returned pointer is into the fiber's
    /// owned stack and is valid for as long as the `Fiber` lives. The
    /// caller is responsible for any unsafety in dereferencing it
    /// (e.g. interpreting stack contents for GC root scanning).
    pub fn saved_fp(&self) -> Option<*const u8> {
        if !matches!(self.state(), FiberState::Suspended) {
            return None;
        }
        // SAFETY: A suspended fiber's `fiber_sp` was written by
        // `krio_fiber_switch`'s save path, which always pushes
        // `SAVED_FRAME_BYTES` of register state. The saved fp slot
        // is at a fixed offset within that region.
        let fp = unsafe { *(self.fiber_sp.add(SAVED_FP_OFFSET) as *const *const u8) };
        Some(fp)
    }

    /// Read the saved return address from a suspended fiber — the
    /// instruction the fiber will resume executing on the next
    /// `resume()`. On x86_64 this is the implicit return address
    /// pushed by the call into `krio_fiber_switch`; on aarch64 it's
    /// the saved `x30` (link register).
    ///
    /// Useful for GCs that need to find the active safepoint for a
    /// suspended fiber's deepest frame (look up `saved_ret` against
    /// the host's registered code-range table to find which compiled
    /// function it falls in, then index into that function's
    /// safepoint metadata).
    ///
    /// Returns `None` if the fiber isn't suspended (see
    /// [`Fiber::saved_fp`] for the same semantics).
    pub fn saved_ret(&self) -> Option<*const u8> {
        if !matches!(self.state(), FiberState::Suspended) {
            return None;
        }
        let ret = unsafe { *(self.fiber_sp.add(SAVED_RET_OFFSET) as *const *const u8) };
        Some(ret)
    }

    /// Continue running the fiber until it yields or terminates.
    ///
    /// # Panics
    /// Panics if the fiber is already in a terminal state (`Done` /
    /// `Errored`).
    pub fn resume(&mut self) -> FiberStep {
        assert!(
            !self.done,
            "Fiber: cannot resume a fiber in {:?} state",
            self.state()
        );

        // Re-wire the trampoline back-pointers in case the Fiber
        // moved between construction and the first resume (which
        // would invalidate the &mut self.done etc. pointers).
        self._trampoline_state.done_flag = &mut self.done as *mut bool;
        self._trampoline_state.errored_flag = &mut self.errored as *mut bool;
        self._trampoline_state.error_payload_slot = &mut self.error_payload as *mut _;
        self._trampoline_state.caller_sp_slot = &mut self.caller_sp as *mut *mut u8;
        self._trampoline_state.fiber_sp_slot = &mut self.fiber_sp as *mut *mut u8;

        self.started = true;

        let prev_active = ACTIVE_TRAMPOLINE.with(|cell| {
            let prev = cell.get();
            cell.set(&mut *self._trampoline_state as *mut _);
            prev
        });
        let prev_fiber = ACTIVE_FIBER.with(|cell| {
            let prev = cell.get();
            cell.set(self as *mut Fiber);
            prev
        });

        // Switch: save host sp into self.caller_sp, load self.fiber_sp.
        unsafe {
            krio_fiber_switch(
                &mut self.caller_sp as *mut *mut u8,
                &self.fiber_sp as *const *mut u8,
            );
        }

        ACTIVE_TRAMPOLINE.with(|cell| cell.set(prev_active));
        ACTIVE_FIBER.with(|cell| cell.set(prev_fiber));

        match (self.done, self.errored) {
            (false, _) => FiberStep::Yielded,
            (true, false) => FiberStep::Done,
            (true, true) => FiberStep::Errored,
        }
    }
}

/// Suspend the currently running fiber and switch back to whoever
/// called [`Fiber::resume`].
///
/// # Panics
/// Panics if called outside of a fiber (i.e. from the host thread).
pub fn yield_now() {
    let state_ptr = ACTIVE_TRAMPOLINE.with(|cell| cell.get());
    assert!(
        !state_ptr.is_null(),
        "yield_now: called outside of any active fiber"
    );

    // SAFETY: `state_ptr` was set by Fiber::resume to point at a
    // Box<TrampolineState> that lives at least until the resume
    // returns. We only read its fields.
    let state = unsafe { &*state_ptr };
    let caller_sp_slot = state.caller_sp_slot;
    let fiber_sp_slot = state.fiber_sp_slot;

    // Save the fiber's current sp into fiber_sp_slot, switch back
    // to caller_sp.
    unsafe {
        krio_fiber_switch(fiber_sp_slot, caller_sp_slot as *const *mut u8);
    }
}

/// Yield from the current fiber, sending `value` out to the host
/// (retrievable via [`Fiber::take_yield_value`]) and returning
/// whatever the host passes in via [`Fiber::resume_with`] on the
/// next call.
///
/// Returns `None` if the host called plain [`Fiber::resume`] (no
/// input set) or if the input couldn't be downcast to `I`.
///
/// # Panics
/// Panics if called outside a fiber.
pub fn yield_value<O: 'static, I: 'static>(value: O) -> Option<I> {
    let fiber_ptr = ACTIVE_FIBER.with(|cell| cell.get());
    assert!(
        !fiber_ptr.is_null(),
        "yield_value: called outside of any active fiber"
    );
    // SAFETY: ACTIVE_FIBER is only ever set to a `&mut Fiber`
    // pointer that lives on the host stack until resume returns.
    unsafe {
        (*fiber_ptr)
            .output_slot
            .set(Some(Box::new(value) as Box<dyn Any + 'static>));
    }
    yield_now();
    let received = unsafe { (*fiber_ptr).input_slot.take() };
    received.and_then(|b| b.downcast::<I>().ok().map(|b| *b))
}

/// `u64` fast path for [`yield_value`]. Stores `value` in the
/// fiber's `output_u64` slot (no `Box<dyn Any>` allocation),
/// suspends, and on resume returns whatever the host passed via
/// [`Fiber::resume_with_u64`].
///
/// Host-side counterpart: [`Fiber::take_yield_u64`] /
/// [`Fiber::resume_with_u64`]. The generic [`yield_value`] /
/// [`Fiber::take_yield_value`] pair stays in place — choose
/// whichever matches your payload type.
///
/// # Panics
/// Panics if called outside a fiber.
pub fn yield_u64(value: u64) -> Option<u64> {
    let fiber_ptr = ACTIVE_FIBER.with(|cell| cell.get());
    assert!(
        !fiber_ptr.is_null(),
        "yield_u64: called outside of any active fiber"
    );
    // SAFETY: same lifetime invariants as `yield_value`.
    unsafe {
        (*fiber_ptr).output_u64.set(Some(value));
    }
    yield_now();
    unsafe { (*fiber_ptr).input_u64.take() }
}

/// `u64` fast path for [`take_input`]. Returns whatever the host
/// passed via [`Fiber::resume_with_u64`].
///
/// # Panics
/// Panics if called outside a fiber.
pub fn take_input_u64() -> Option<u64> {
    let fiber_ptr = ACTIVE_FIBER.with(|cell| cell.get());
    assert!(
        !fiber_ptr.is_null(),
        "take_input_u64: called outside of any active fiber"
    );
    unsafe { (*fiber_ptr).input_u64.take() }
}

/// Take whatever input the host most recently passed via
/// [`Fiber::resume_with`]. Useful for the "first call" case where
/// `yield_value` hasn't been invoked yet but the closure wants to
/// see the initial argument.
///
/// Returns `None` if the host used plain [`Fiber::resume`] or if
/// the input couldn't be downcast to `I`.
///
/// # Panics
/// Panics if called outside a fiber.
pub fn take_input<I: 'static>() -> Option<I> {
    let fiber_ptr = ACTIVE_FIBER.with(|cell| cell.get());
    assert!(
        !fiber_ptr.is_null(),
        "take_input: called outside of any active fiber"
    );
    let received = unsafe { (*fiber_ptr).input_slot.take() };
    received.and_then(|b| b.downcast::<I>().ok().map(|b| *b))
}

/// Process-unique id of the currently running fiber, or `None` if
/// invoked from the host thread.
pub fn current_fiber_id() -> Option<u64> {
    let fiber_ptr = ACTIVE_FIBER.with(|cell| cell.get());
    if fiber_ptr.is_null() {
        return None;
    }
    Some(unsafe { (*fiber_ptr).id })
}

/// True if the currently running fiber has been cancelled. Returns
/// `false` from the host thread (never cancelled).
///
/// # Panics
/// Does not panic — returns `false` outside any fiber. Different
/// from [`yield_now`] which panics, because cancellation polling is
/// idiomatic in helper functions that may be called from either
/// fiber or non-fiber contexts.
pub fn is_cancelled() -> bool {
    let fiber_ptr = ACTIVE_FIBER.with(|cell| cell.get());
    if fiber_ptr.is_null() {
        return false;
    }
    // SAFETY: `fiber_ptr` is set by `Fiber::resume` to `&mut *self`
    // and is restored on return. Only this thread can see it.
    unsafe { (*fiber_ptr).cancelled.get() }
}

/// True if the currently running fiber has a deadline that has
/// already passed. Returns `false` from the host thread.
pub fn is_deadline_passed() -> bool {
    let fiber_ptr = ACTIVE_FIBER.with(|cell| cell.get());
    if fiber_ptr.is_null() {
        return false;
    }
    unsafe { (*fiber_ptr).is_deadline_passed() }
}

/// True if the currently running fiber should yield/return early —
/// i.e. it's been cancelled, or its deadline has passed. The common
/// "cooperative bail" check.
pub fn should_yield_early() -> bool {
    is_cancelled() || is_deadline_passed()
}

/// Current Unix time in milliseconds. Used for deadline checks.
fn current_time_ms() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// Lay out a fresh fiber stack so a `krio_fiber_switch` *into* it
/// returns at the trampoline. Returns the initial saved-sp value.
///
/// # Safety
/// `stack` must outlive the returned pointer. `state` must remain
/// valid (heap-pinned) for the fiber's lifetime.
unsafe fn prepare_initial_stack(stack: &mut [u8], state: *mut TrampolineState) -> *mut u8 {
    // Stack grows down. Top of stack is `&stack[stack.len()]`.
    let top = unsafe { stack.as_mut_ptr().add(stack.len()) };
    // Align top to 16 bytes (aarch64 + x86_64 ABI).
    let top = ((top as usize) & !0xF) as *mut u8;

    unsafe {
        // Architecture-specific: place the trampoline as the
        // "return address" the saved frame pops on switch-in,
        // followed by zeroed callee-saved register slots.
        #[cfg(all(target_arch = "x86_64", windows))]
        {
            let stack_limit = stack.as_mut_ptr();
            prepare_initial_stack_arch(top, stack_limit, state)
        }
        #[cfg(not(all(target_arch = "x86_64", windows)))]
        prepare_initial_stack_arch(top, state)
    }
}

/// Default MXCSR: all six SSE exceptions masked, round-to-nearest,
/// flush-to-zero off. This is what the SysV process startup state and
/// the CRT both install. **Must not be left as zero** — a zero MXCSR
/// unmasks every SSE exception, so the first denormal or inexact
/// result inside a brand-new fiber would raise SIGFPE.
#[cfg(target_arch = "x86_64")]
const DEFAULT_MXCSR: u32 = 0x1F80;

/// Default x87 control word: extended precision, round-to-nearest,
/// all six x87 exceptions masked. Same reasoning as [`DEFAULT_MXCSR`]
/// — zero here would unmask the x87 exception set.
#[cfg(target_arch = "x86_64")]
const DEFAULT_X87_CW: u16 = 0x037F;

#[cfg(all(target_arch = "x86_64", not(windows)))]
unsafe fn prepare_initial_stack_arch(top: *mut u8, state: *mut TrampolineState) -> *mut u8 {
    // SysV saved-frame layout (low → high addresses), mirroring the
    // save path in `crate::arch`:
    //   sp+0   : MXCSR            (4 bytes)
    //   sp+4   : x87 control word (2 bytes)
    //   sp+8   : pad
    //   sp+16  : r15, r14, r13, r12, rbx, rbp
    //   sp+64  : trampoline_addr (the saved return addr)
    // Total: 72 bytes from sp to the top of the frame.
    //
    // `state` is stashed in the r12 slot (sp+40) — r12 is callee-saved
    // on SysV, so the trampoline observes it on first entry.
    //
    // x86_64 SysV wants `%rsp % 16 == 8` on function entry because the
    // `call` pushed an 8-byte return address. `top` is 16-aligned and
    // `top - 72 ≡ 8 (mod 16)`, which is exactly the residue the switch's
    // own save path produces, so the two agree.
    let sp = unsafe { top.sub(SAVED_FRAME_BYTES + 8) };
    unsafe {
        core::ptr::write_bytes(sp, 0, SAVED_FRAME_BYTES);
        // FP control state. Seeded with the ABI defaults rather than
        // zero — see DEFAULT_MXCSR.
        (sp as *mut u32).write(DEFAULT_MXCSR);
        (sp.add(4) as *mut u16).write(DEFAULT_X87_CW);
        // Trampoline state for r12.
        (sp.add(40) as *mut usize).write(state as usize);
        // ret_addr slot — trampoline entry point.
        (sp.add(SAVED_RET_OFFSET) as *mut usize)
            .write(fiber_trampoline_x86_64 as *const () as usize);
    }
    sp
}

#[cfg(all(target_arch = "x86_64", windows))]
unsafe fn prepare_initial_stack_arch(
    top: *mut u8,
    stack_limit: *mut u8,
    state: *mut TrampolineState,
) -> *mut u8 {
    // MS x64 saved-frame layout (low → high addresses):
    //   sp+0             : MXCSR (4 bytes)
    //   sp+4             : x87 control word (2 bytes)
    //   sp+8             : pad
    //   sp+16  .. sp+160 : xmm6..xmm15 (10 × 16 bytes = 160)
    //   sp+176 .. sp+232 : r15, r14, r13, r12, rsi, rdi, rbx, rbp
    //   sp+240           : TEB.StackLimit save slot
    //   sp+248           : TEB.StackBase save slot
    //   sp+256           : trampoline_addr (the saved return addr)
    // Total: 264 bytes from sp to (top-of-frame).
    //
    // `state` is stashed in the r12 slot (sp+200) — the MS x64
    // callee-saved set includes r12, so the trampoline observes it
    // on first entry. The TEB slots are seeded so that the first
    // switch-in writes the fiber's stack range to gs:[0x08]/[0x10],
    // which lets SEH walk through the fiber's frames (necessary for
    // catch_unwind in `fiber_run`).
    let sp = unsafe { top.sub(SAVED_FRAME_BYTES + 8) };
    unsafe {
        core::ptr::write_bytes(sp, 0, SAVED_FRAME_BYTES);
        // FP control state. Seeded with the ABI defaults rather than
        // zero — see DEFAULT_MXCSR.
        (sp as *mut u32).write(DEFAULT_MXCSR);
        (sp.add(4) as *mut u16).write(DEFAULT_X87_CW);
        // Trampoline state for r12.
        (sp.add(200) as *mut usize).write(state as usize);
        // TEB.StackLimit: the low end of the fiber's usable stack.
        (sp.add(240) as *mut usize).write(stack_limit as usize);
        // TEB.StackBase: the high end (= the aligned `top`).
        (sp.add(248) as *mut usize).write(top as usize);
        // ret_addr slot — trampoline entry point.
        (sp.add(SAVED_RET_OFFSET) as *mut usize)
            .write(fiber_trampoline_x86_64 as *const () as usize);
    }
    sp
}

#[cfg(all(target_arch = "aarch64", not(windows)))]
unsafe fn prepare_initial_stack_arch(top: *mut u8, state: *mut TrampolineState) -> *mut u8 {
    // The switch's restore sequence wants the stack (low to high) to
    // look like the saved frame produced by the asm save:
    //   [x19][x20][x21][x22][x23][x24][x25][x26][x27][x28][x29][x30]
    //   [d8..d15][fpcr][pad]
    // x30 is the return address — set it to the trampoline.
    // Stash `state` in x19 so the trampoline can recover it.
    let sp = unsafe { top.sub(SAVED_FRAME_BYTES) };
    unsafe {
        // x19 at offset 0
        (sp as *mut usize).write(state as usize);
        // x20..x29 zeroed (offsets 8..88)
        for i in 1..11 {
            (sp.add(i * 8) as *mut usize).write(0);
        }
        // x30 (return address) at offset 88
        (sp.add(88) as *mut usize).write(fiber_trampoline_aarch64 as *const () as usize);
        // d8-d15 slots at [96, 160), FPCR at 160, pad to 192: zero
        // explicitly so the first restore loads clean FP state even on
        // non-mmap stacks. Unlike x86_64's MXCSR, an all-zero AArch64
        // FPCR *is* the sane default — round-to-nearest with every
        // exception trap disabled — so no seeding is needed here.
        for i in 12..24 {
            (sp.add(i * 8) as *mut usize).write(0);
        }
    }
    sp
}

// Unsupported targets (wasm32, riscv64, aarch64-windows, …): no native
// stack to lay out. Compiles so the crate builds where it is pulled in
// transitively. Note this panics from `Fiber::with_stack_size`, i.e.
// `Fiber::new` itself fails loudly on these targets rather than handing
// back a fiber that dies later.
#[cfg(not(any(target_arch = "x86_64", all(target_arch = "aarch64", not(windows)))))]
unsafe fn prepare_initial_stack_arch(_top: *mut u8, _state: *mut TrampolineState) -> *mut u8 {
    panic!("krio-fiber: native fibers are unavailable on this target");
}

// ── Trampolines ───────────────────────────────────────────────────

#[cfg(all(target_arch = "x86_64", not(windows)))]
#[unsafe(naked)]
unsafe extern "C" fn fiber_trampoline_x86_64() {
    // r12 holds the TrampolineState pointer (stashed in
    // prepare_initial_stack_arch). Move it into rdi and tail-call
    // `fiber_run`.
    core::arch::naked_asm!("mov %r12, %rdi", "call {f}", "ud2",
        f = sym fiber_run,
        options(att_syntax),
    )
}

#[cfg(all(target_arch = "x86_64", windows))]
#[unsafe(naked)]
unsafe extern "C" fn fiber_trampoline_x86_64() {
    // MS x64: first arg goes in rcx, and the caller must reserve
    // 32 bytes of shadow space below the call. `sub $32, %rsp`
    // keeps rsp 16-aligned (the `ret` that entered this function
    // left rsp 16-aligned) so the subsequent `call` pushes 8 bytes
    // for the return address and `fiber_run` sees the ABI-required
    // rsp % 16 == 8 on entry.
    core::arch::naked_asm!(
        "mov %r12, %rcx",
        "sub $32, %rsp",
        "call {f}",
        "ud2",
        f = sym fiber_run,
        options(att_syntax),
    )
}

#[cfg(all(target_arch = "aarch64", not(windows)))]
#[unsafe(naked)]
unsafe extern "C" fn fiber_trampoline_aarch64() {
    // x19 holds the TrampolineState pointer. Move to x0 and call
    // `fiber_run`. ud2 / brk on return — fiber_run never returns.
    core::arch::naked_asm!("mov x0, x19", "bl {f}", "brk #0",
        f = sym fiber_run,
    )
}

/// The body of a fiber. Receives a pointer to the trampoline state,
/// runs the closure, marks the fiber done, and switches back to the
/// caller. Never returns to its caller (the trampoline's tail).
#[cfg_attr(
    not(any(target_arch = "x86_64", all(target_arch = "aarch64", not(windows)))),
    allow(dead_code)
)]
extern "C" fn fiber_run(state_ptr: *mut TrampolineState) -> ! {
    // SAFETY: `state_ptr` is the heap-allocated state from Fiber::new
    // / Fiber::resume; valid for the fiber's lifetime.
    let state = unsafe { &mut *state_ptr };

    // Take the closure out of the state. `catch_unwind` is mandatory
    // here — letting a panic unwind across the asm switch is UB
    // (the unwinder follows DWARF, which doesn't know our switch
    // happened). Captured payload is parked on the parent fiber so
    // the host can retrieve it via `Fiber::take_error`.
    if let Some(closure) = state.closure.take() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(closure));
        if let Err(panic) = result {
            unsafe {
                *state.errored_flag = true;
                *state.error_payload_slot = Some(panic);
            }
        }
    }

    // Mark done and switch back to the caller. This call never
    // returns — there's nowhere to come back to.
    unsafe {
        *state.done_flag = true;
        loop {
            krio_fiber_switch(state.fiber_sp_slot, state.caller_sp_slot as *const *mut u8);
            // If somehow control returns here (consumer re-resumed
            // a Done fiber via the asm directly), keep yielding.
        }
    }
}

// ── Task impl ─────────────────────────────────────────────────────

impl Task for Fiber {
    fn step(&mut self) -> Suspension {
        match self.resume() {
            FiberStep::Yielded => Suspension::Yielded,
            // Errored fibers are "done" from the scheduler's
            // perspective — they won't make further progress.
            FiberStep::Done | FiberStep::Errored => Suspension::Completed,
        }
    }
}
