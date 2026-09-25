//! Allocation-free contexts for hosts without `std` or a heap (kernels,
//! bare metal). The host provides every stack, and any context can
//! switch to any other: there is no parent to return to, so a
//! scheduler can hand control straight from one context to the next.

use crate::arch::krio_fiber_switch;
use crate::frame::initial_frame;

/// A suspended execution context: the stack pointer its last switch
/// saved, or the first frame of a context that has not run yet.
pub struct Context {
    sp: *mut u8,
}

/// Where a fresh context starts, kept at the top of its own stack.
#[repr(C)]
struct Start {
    entry: extern "C" fn(*mut u8) -> !,
    arg: *mut u8,
}

impl Context {
    /// The context of whatever is running when it first switches away,
    /// such as the host's own stack; [`switch`] fills it in.
    pub const fn empty() -> Self {
        Context {
            sp: core::ptr::null_mut(),
        }
    }

    /// A context that runs `entry(arg)` on `stack` when first switched
    /// to. `entry` never returns: when its work is done it switches to
    /// another context and is not resumed again.
    ///
    /// # Safety
    /// `stack` must stay valid, and be used for nothing else, while the
    /// context can still run.
    pub unsafe fn new(stack: &mut [u8], entry: extern "C" fn(*mut u8) -> !, arg: *mut u8) -> Self {
        unsafe {
            let top = stack.as_mut_ptr().add(stack.len());
            let top = ((top as usize) & !0xF) as *mut u8;
            // The start record sits at the top; the frame goes below it,
            // still 16-aligned.
            let start = top.sub(core::mem::size_of::<Start>().next_multiple_of(16)) as *mut Start;
            start.write(Start { entry, arg });
            let frame_top = start as *mut u8;
            #[cfg(all(windows, any(target_arch = "x86_64", target_arch = "aarch64")))]
            let sp = initial_frame(
                frame_top,
                stack.as_mut_ptr(),
                start as usize,
                trampoline_addr(),
            );
            #[cfg(not(all(windows, any(target_arch = "x86_64", target_arch = "aarch64"))))]
            let sp = initial_frame(frame_top, start as usize, trampoline_addr());
            Context { sp }
        }
    }
}

/// Save the running context into `from` and resume `to`.
///
/// # Safety
/// `to` must be a context made by [`Context::new`] or saved by an
/// earlier `switch`, and must not be the one running.
pub unsafe fn switch(from: &mut Context, to: &Context) {
    unsafe { krio_fiber_switch(&mut from.sp, &to.sp) }
}

extern "C" fn start(record: *const Start) -> ! {
    let Start { entry, arg } = unsafe { record.read() };
    entry(arg)
}

fn trampoline_addr() -> usize {
    trampoline as *const () as usize
}

// Each trampoline moves the start record out of the callee-saved
// register the first frame put it in and calls `start`, as the `Fiber`
// trampolines do for their state.

#[cfg(all(target_arch = "x86_64", not(windows)))]
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    core::arch::naked_asm!("mov %r12, %rdi", "call {f}", "ud2",
        f = sym start,
        options(att_syntax),
    )
}

#[cfg(all(target_arch = "x86_64", windows))]
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    // Shadow space for the MS x64 call, keeping rsp 16-aligned.
    core::arch::naked_asm!("mov %r12, %rcx", "sub $32, %rsp", "call {f}", "ud2",
        f = sym start,
        options(att_syntax),
    )
}

#[cfg(all(target_arch = "x86", not(windows)))]
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    // cdecl: the argument goes on the stack, 16-aligned at the call.
    core::arch::naked_asm!("sub $8, %esp", "push %ebx", "call {f}", "ud2",
        f = sym start,
        options(att_syntax),
    )
}

#[cfg(target_arch = "riscv64")]
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    core::arch::naked_asm!("mv a0, s1", "call {f}", "ebreak", f = sym start)
}

#[cfg(all(target_arch = "aarch64", not(windows)))]
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    core::arch::naked_asm!("mov x0, x19", "bl {f}", "brk #0", f = sym start)
}

#[cfg(all(target_arch = "aarch64", windows))]
#[unsafe(naked)]
unsafe extern "C" fn trampoline() {
    // Null x30 so SEH's unwind stops here (see the Fiber trampoline).
    core::arch::naked_asm!("mov x0, x19", "mov x30, xzr", "b {f}", "brk #0", f = sym start)
}
