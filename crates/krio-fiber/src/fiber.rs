//! Fiber type + the global "currently active fiber" hook that
//! [`yield_now`] uses to swap back to the caller.

use std::cell::Cell;

use krio_core::{Suspension, Task};

use crate::arch::{SAVED_FRAME_BYTES, krio_fiber_switch};

/// Result of a single [`Fiber::resume`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiberStep {
    /// Fiber yielded; another `resume` will continue it.
    Yielded,
    /// Fiber's entry closure returned; further `resume` calls panic.
    Done,
}

/// A stackful coroutine. Owns its stack; runs an `FnOnce` closure
/// that may call [`yield_now`] from any depth to suspend back to the
/// caller of [`Fiber::resume`].
///
/// Single-threaded — fibers are explicitly `!Send`. A multi-threaded
/// variant could lift this restriction once the scheduling story is
/// concrete.
pub struct Fiber {
    /// Fiber's stack. Kept alive for the fiber's lifetime; freed on
    /// drop. `_stack[0]` is the lowest address; the fiber grows the
    /// stack downward from `_stack[len-1]`.
    _stack: Box<[u8]>,
    /// Current saved stack pointer for the fiber. On `resume`, we
    /// load from here; on `yield_now`, we save into here.
    fiber_sp: *mut u8,
    /// Saved stack pointer for the caller (where `resume` was
    /// invoked from). Populated on each `resume` call.
    caller_sp: *mut u8,
    /// Set to `true` by the trampoline when the user closure returns.
    done: bool,
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
    closure: Option<Box<dyn FnOnce()>>,
    /// Pointer to the parent fiber's `done` flag. Set after the
    /// closure returns.
    done_flag: *mut bool,
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
}

/// Default fiber stack size — 64 KB is comfortable for nested calls
/// while still cheap to allocate. Tune via [`Fiber::with_stack_size`]
/// if you know your workload.
pub const DEFAULT_STACK_SIZE: usize = 64 * 1024;

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
        let mut stack = vec![0u8; aligned].into_boxed_slice();

        let mut state = Box::new(TrampolineState {
            closure: Some(Box::new(f) as Box<dyn FnOnce()>),
            done_flag: std::ptr::null_mut(),
            caller_sp_slot: std::ptr::null_mut(),
            fiber_sp_slot: std::ptr::null_mut(),
        });

        let fiber_sp = unsafe { prepare_initial_stack(&mut stack, &mut *state as *mut _) };

        let mut fiber = Fiber {
            _stack: stack,
            fiber_sp,
            caller_sp: std::ptr::null_mut(),
            done: false,
            _trampoline_state: state,
            _not_send: std::marker::PhantomData,
        };

        // Wire the trampoline back-pointers now that the fiber
        // struct's address is stable on the heap... wait, the fiber
        // itself is on the stack here. Move the wiring into resume(),
        // before the first switch.
        fiber._trampoline_state.done_flag = &mut fiber.done as *mut bool;
        fiber._trampoline_state.caller_sp_slot = &mut fiber.caller_sp as *mut *mut u8;
        fiber._trampoline_state.fiber_sp_slot = &mut fiber.fiber_sp as *mut *mut u8;

        fiber
    }

    /// Whether the fiber has completed (will panic on further
    /// `resume` calls).
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Continue running the fiber until it yields or returns.
    ///
    /// # Panics
    /// Panics if the fiber has already returned (`Done` from a
    /// previous resume).
    pub fn resume(&mut self) -> FiberStep {
        assert!(!self.done, "Fiber: cannot resume a fiber that has Done");

        // Re-wire the trampoline back-pointers in case the Fiber
        // moved between construction and the first resume (which
        // would invalidate the &mut self.done etc. pointers).
        self._trampoline_state.done_flag = &mut self.done as *mut bool;
        self._trampoline_state.caller_sp_slot = &mut self.caller_sp as *mut *mut u8;
        self._trampoline_state.fiber_sp_slot = &mut self.fiber_sp as *mut *mut u8;

        let prev_active = ACTIVE_TRAMPOLINE.with(|cell| {
            let prev = cell.get();
            cell.set(&mut *self._trampoline_state as *mut _);
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

        if self.done {
            FiberStep::Done
        } else {
            FiberStep::Yielded
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
        prepare_initial_stack_arch(top, state)
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn prepare_initial_stack_arch(top: *mut u8, state: *mut TrampolineState) -> *mut u8 {
    // The switch's `pop %r15..%rbp; ret` sequence wants the stack
    // (low to high) to look like:
    //   [r15][r14][r13][r12][rbx][rbp][trampoline_addr]
    // So we push trampoline_addr first (highest of the seven slots),
    // then six zeroed register slots below it. Final sp points at
    // the bottom (the r15 slot).
    let mut sp = top;
    unsafe {
        // x86_64 SysV: rsp must be 16-byte aligned + 8 (i.e.
        // %rsp % 16 == 8) on function entry, because `call` would
        // have pushed the 8-byte return address. Our `ret` will
        // pop the trampoline addr → equivalent to entering the
        // trampoline as a normal function call.
        // After the 7 quadwords below, sp is `top - 56`. For 16-byte
        // alignment + 8, we need top to be 16-aligned (it is — we
        // aligned above).
        sp = sp.sub(8);
        (sp as *mut usize).write(fiber_trampoline_x86_64 as *const () as usize);
        // 6 callee-saved register slots, zeroed.
        for _ in 0..6 {
            sp = sp.sub(8);
            (sp as *mut usize).write(0);
        }
        // Stash `state` somewhere the trampoline can find it.
        // x86_64: we'll pass it via rdi by emitting a tiny preamble
        // in the trampoline that loads it from a known TLS / global
        // location. Simpler: stash it in r12 (callee-saved) by
        // overwriting the r12 slot.
        // The pop order is r15, r14, r13, r12, rbx, rbp — so the
        // r12 slot is the 4th from the bottom (offset 16 from sp).
        let r12_slot = sp.add(8 * 3) as *mut usize;
        r12_slot.write(state as usize);
    }
    sp
}

#[cfg(target_arch = "aarch64")]
unsafe fn prepare_initial_stack_arch(top: *mut u8, state: *mut TrampolineState) -> *mut u8 {
    // The switch's restore sequence wants the stack (low to high) to
    // look like the saved frame produced by the asm save:
    //   [x19][x20][x21][x22][x23][x24][x25][x26][x27][x28][x29][x30][pad]
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
        // pad slot at offset 96, leave zeroed
    }
    sp
}

// ── Trampolines ───────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
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

#[cfg(target_arch = "aarch64")]
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
extern "C" fn fiber_run(state_ptr: *mut TrampolineState) -> ! {
    // SAFETY: `state_ptr` is the heap-allocated state from Fiber::new
    // / Fiber::resume; valid for the fiber's lifetime.
    let state = unsafe { &mut *state_ptr };

    // Take the closure out of the state. Catch panics so a panicking
    // fiber doesn't unwind through the asm switch (which would be UB).
    if let Some(closure) = state.closure.take() {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(closure));
        if let Err(panic) = result {
            // We don't have a great place to surface this in the
            // current API. Print and abort — better than silently
            // continuing or unwinding through asm.
            eprintln!("krio-fiber: fiber panicked: {panic:?}");
            std::process::abort();
        }
    }

    // Mark done and switch back to the caller. This call never
    // returns — there's nowhere to come back to.
    unsafe {
        *state.done_flag = true;
        loop {
            krio_fiber_switch(
                state.fiber_sp_slot,
                state.caller_sp_slot as *const *mut u8,
            );
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
            FiberStep::Done => Suspension::Completed,
        }
    }
}
