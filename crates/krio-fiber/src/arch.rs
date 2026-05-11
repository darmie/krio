//! Target-specific context switch.
//!
//! Each architecture exports a single `extern "C"` symbol
//! `krio_fiber_switch(save_to: *mut *mut u8, load_from: *const *mut u8)`:
//!
//! - Save callee-saved registers + the current return address onto
//!   the current stack.
//! - Write the resulting stack pointer through `save_to`.
//! - Load the new stack pointer from `load_from`.
//! - Pop callee-saved registers + return — control resumes at
//!   whatever return address sat on top of the new stack.
//!
//! The "initial state" of a fresh fiber's stack is constructed by
//! [`super::fiber::prepare_initial_stack`] to look like a saved
//! frame whose return address points at the fiber's trampoline.

use core::arch::global_asm;

unsafe extern "C" {
    /// Save the current context onto the current stack, then switch
    /// to the context whose stack pointer is at `*load_from`. Writes
    /// the saved-stack pointer into `*save_to`.
    ///
    /// # Safety
    /// `save_to` and `load_from` must be valid pointers. The stack
    /// being switched to must contain a saved-frame layout produced
    /// either by an earlier call to this function or by
    /// [`super::fiber::prepare_initial_stack`].
    pub fn krio_fiber_switch(save_to: *mut *mut u8, load_from: *const *mut u8);
}

#[cfg(target_arch = "x86_64")]
global_asm!(
    r#"
    .global _krio_fiber_switch
    .global krio_fiber_switch
    _krio_fiber_switch:
    krio_fiber_switch:
        push   %rbp
        push   %rbx
        push   %r12
        push   %r13
        push   %r14
        push   %r15
        mov    %rsp, (%rdi)
        mov    (%rsi), %rsp
        pop    %r15
        pop    %r14
        pop    %r13
        pop    %r12
        pop    %rbx
        pop    %rbp
        ret
    "#,
    options(att_syntax)
);

#[cfg(target_arch = "aarch64")]
global_asm!(
    r#"
    .global _krio_fiber_switch
    .global krio_fiber_switch
    _krio_fiber_switch:
    krio_fiber_switch:
        sub  sp, sp, #112
        stp  x19, x20, [sp, #0]
        stp  x21, x22, [sp, #16]
        stp  x23, x24, [sp, #32]
        stp  x25, x26, [sp, #48]
        stp  x27, x28, [sp, #64]
        stp  x29, x30, [sp, #80]
        // sp slot at [96] reserved for alignment
        mov  x9, sp
        str  x9, [x0]
        ldr  x9, [x1]
        mov  sp, x9
        ldp  x19, x20, [sp, #0]
        ldp  x21, x22, [sp, #16]
        ldp  x23, x24, [sp, #32]
        ldp  x25, x26, [sp, #48]
        ldp  x27, x28, [sp, #64]
        ldp  x29, x30, [sp, #80]
        add  sp, sp, #112
        ret
    "#
);

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!(
    "krio-fiber: context switch not yet implemented for this target. \
     Supported: x86_64, aarch64."
);

/// Number of bytes [`krio_fiber_switch`] pushes onto the stack
/// during a save. Used by [`super::fiber::prepare_initial_stack`]
/// to lay out the fake saved frame for a brand-new fiber.
#[cfg(target_arch = "x86_64")]
pub const SAVED_FRAME_BYTES: usize = 6 * 8; // rbp, rbx, r12, r13, r14, r15

#[cfg(target_arch = "aarch64")]
pub const SAVED_FRAME_BYTES: usize = 112; // 12 callee-saved regs + alignment slot

/// Byte offset, from a suspended fiber's [`super::Fiber::saved_sp`],
/// of the saved frame-pointer register (`rbp` on x86_64, `x29` on
/// aarch64). The suspended fiber's stack frame chain starts at
/// `*(saved_sp + SAVED_FP_OFFSET)`. Used by host GCs to walk the
/// fiber's frames when scanning roots across a suspension.
#[cfg(target_arch = "x86_64")]
pub const SAVED_FP_OFFSET: usize = 40; // r15,r14,r13,r12,rbx then rbp at +40

#[cfg(target_arch = "aarch64")]
pub const SAVED_FP_OFFSET: usize = 80; // x29 lives at sp+80 (see stp pair)

/// Byte offset of the saved return address (instruction at which the
/// fiber will resume execution after the next context switch). On
/// x86_64 this is the implicit return address pushed by the `call`
/// to `krio_fiber_switch` and sits just above the saved registers.
/// On aarch64 this is `x30`, saved alongside `x29` in the stp pair.
#[cfg(target_arch = "x86_64")]
pub const SAVED_RET_OFFSET: usize = 48; // ret_addr sits above the 6 saved regs

#[cfg(target_arch = "aarch64")]
pub const SAVED_RET_OFFSET: usize = 88; // x30 lives at sp+88 (companion of x29)
