//! Target-specific context switch.
//!
//! Each architecture exports a single `extern "C"` symbol
//! `krio_fiber_switch(save_to: *mut *mut u8, load_from: *const *mut u8)`:
//!
//! - Save callee-saved registers, the callee-saved floating-point
//!   control/rounding state, and the current return address onto
//!   the current stack.
//! - Write the resulting stack pointer through `save_to`.
//! - Load the new stack pointer from `load_from`.
//! - Pop callee-saved registers + return — control resumes at
//!   whatever return address sat on top of the new stack.
//!
//! The "initial state" of a fresh fiber's stack is constructed by
//! [`super::fiber::prepare_initial_stack`] to look like a saved
//! frame whose return address points at the fiber's trampoline.
//!
//! ## Floating-point control state
//!
//! Every ABI we implement makes the FP control/rounding word
//! callee-saved: the SysV AMD64 psABI §3.2.1 for the MXCSR *control*
//! bits and the x87 control word, MS x64 for the same pair, and
//! AAPCS64 for `FPCR`. A switch that does not spill them lets a
//! fiber's rounding mode (or its exception masks) leak into whoever
//! resumed it — e.g. a native library that switches to
//! round-toward-zero and then yields silently changes the host's
//! arithmetic. They therefore ride the saved frame alongside the
//! callee-saved registers.
//!
//! ## Supported targets
//!
//! `x86_64` (SysV and MS x64) and `aarch64` on non-Windows targets.
//! `aarch64-pc-windows-*` is deliberately *not* claimed: the AAPCS64
//! switch below is ABI-correct for the registers, but ARM64 Windows
//! also requires swapping the per-thread TEB stack bounds (see the MS
//! x64 block) or SEH refuses to unwind out of a fiber, and
//! [`super::stack`] has no guard-page allocator there either. Rather
//! than compile into a switch that works until the first panic
//! crosses a fiber boundary, ARM64 Windows takes the unsupported
//! path and panics honestly.
//!
//! ### The TEB swap alone is not enough — measured
//!
//! An earlier revision of this comment said the port "is not large":
//! ARM64 Windows keeps the TEB pointer in `x18` (which the AAPCS64 asm
//! below correctly never touches, since Windows reserves it), so the
//! stack-bounds swap is a pair of `ldr`/`str` through `x18` — StackBase
//! at `[x18, #8]`, StackLimit at `[x18, #16]` — instead of the
//! `gs:`-relative moves used on x64. That much is true, and it was
//! written and run on a `windows-11-arm` runner.
//!
//! It is not sufficient. With the swap in place the whole suite passes
//! in **debug** and dies in **release** with exit code `0xe06d7363` —
//! the MS C++ EH code, raised on the first test that panics inside a
//! fiber. Which is exactly the failure this stub exists to prevent, so
//! the switch was reverted rather than shipped.
//!
//! Why debug passes and release does not is **not established**. One CI
//! run is one data point, and the mechanism was never instrumented.
//! Three candidates, with what argues for and against each:
//!
//! 1. **Inlining changes where the unwind walk ends up.** Debug keeps
//!    real frames between the panic and `fiber_run`'s `catch_unwind`;
//!    release collapses the closure into it. If the phase-1 handler
//!    search reaches one frame further in release it meets the naked
//!    trampoline and the synthetic bottom frame — and neither asm block
//!    here carries `.pdata`. On x64 a function without unwind data
//!    unwinds as a leaf with the return address at `[rsp]`, which the
//!    synthetic frame happens to populate; ARM64's leaf fallback takes
//!    it from `x30`, which at that point holds whatever the switch
//!    left. This is the only candidate that explains both the
//!    architecture difference and the profile difference.
//! 2. **Stack exhaustion.** Windows fiber stacks are a plain
//!    `Box<[u8]>` with no guard page (see `super::stack`) and default
//!    to 64 KB; panic and unwinder machinery is stack-hungry, and an
//!    overrun would corrupt the heap rather than trap. It explains why
//!    only the panicking tests fail — but it argues the wrong way on
//!    profile, since debug frames are larger and debug passed.
//! 3. **Packed `.pdata`.** ARM64 Windows permits a compact unwind
//!    encoding for simple prologues, which release code is likelier to
//!    qualify for. Least likely; it would be a codegen bug.
//!
//! Each is one CI run to discriminate: raise the panic tests'
//! `with_stack_size` to 1 MB (tests 2), build release at
//! `-C opt-level=1` (tests 1), or add `.seh_proc` /
//! `.seh_endprologue` / `.seh_endproc` to both asm blocks (tests 1
//! directly). Do that before writing any more assembly.
//!
//! Debug passing is not evidence of correctness here; it is the same
//! trap as before, one optimisation level lower.

#[cfg(any(
    target_arch = "x86_64",
    all(target_arch = "x86", not(windows)),
    target_arch = "riscv64",
    all(target_arch = "aarch64", not(windows))
))]
use core::arch::global_asm;

#[cfg(any(
    target_arch = "x86_64",
    all(target_arch = "x86", not(windows)),
    target_arch = "riscv64",
    all(target_arch = "aarch64", not(windows))
))]
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

// SysV x86_64: args land in rdi/rsi and the callee-saved set is
// {rbp, rbx, r12..r15}. There are no callee-saved xmm registers on
// SysV, but the MXCSR *control* bits and the x87 control word are
// callee-saved (psABI §3.2.1), so those ride the frame too. Used on
// Linux, macOS, and any other non-Windows x86_64 target.
//
// Saved frame, low → high address, from the saved sp:
//   sp+0   : MXCSR      (4 bytes)
//   sp+4   : x87 CW     (2 bytes, 2 bytes of pad after it)
//   sp+8   : pad — keeps the 16-byte granularity of the sub
//   sp+16  : r15, r14, r13, r12, rbx, rbp   (6 × 8 = 48)
//   sp+64  : return address pushed by the `call`
#[cfg(all(target_arch = "x86_64", not(windows)))]
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
        sub     $16, %rsp
        stmxcsr (%rsp)
        fnstcw  4(%rsp)
        mov    %rsp, (%rdi)
        mov    (%rsi), %rsp
        ldmxcsr (%rsp)
        fldcw   4(%rsp)
        add     $16, %rsp
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

// Microsoft x64 (Windows): args land in rcx/rdx and the callee-saved
// set is {rbp, rbx, rdi, rsi, r12..r15} plus xmm6..xmm15. We save
// the eight callee-saved GP regs (8 × 8 = 64 bytes), the ten
// non-volatile xmm regs (10 × 16 = 160 bytes) and the callee-saved
// MXCSR / x87 control pair (16 bytes, of which 6 are used).
//
// We also swap the per-thread TEB fields the Windows runtime uses to
// bound stack walks:
//   TEB.StackBase  at  gs:[0x08]  (high stack address)
//   TEB.StackLimit at  gs:[0x10]  (low committed address)
// SEH (`RtlLookupFunctionEntry`, `RtlUnwindEx`) refuses to walk past
// rsp values outside `[StackLimit, StackBase]`. Without the swap,
// `catch_unwind` in `fiber_run` can't catch a panic raised inside a
// fiber — the unwinder bails as soon as rsp moves into the fiber's
// heap-allocated stack and the panic terminates the process with the
// MS C++ EH exit code 0xe06d7363.
//
// The fiber's StackBase / StackLimit are seeded by
// `super::fiber::prepare_initial_stack`. On every switch we save the
// outgoing side's current TEB values onto its own stack and load the
// incoming side's saved values into the TEB. Total saved-frame size
// is therefore 256 bytes: 16 (FP control) + 160 (xmm) + 64 (GP) +
// 16 (TEB pair).
//
// `TEB.DeallocationStack` at gs:[0x1478] is left untouched. It's
// only consulted by Win32 thread-cleanup paths the runtime never
// hits in-fiber.
#[cfg(all(target_arch = "x86_64", windows))]
global_asm!(
    r#"
    .global krio_fiber_switch
    krio_fiber_switch:
        movq   %gs:0x08, %rax
        push   %rax
        movq   %gs:0x10, %rax
        push   %rax
        push   %rbp
        push   %rbx
        push   %rdi
        push   %rsi
        push   %r12
        push   %r13
        push   %r14
        push   %r15
        sub    $176, %rsp
        stmxcsr 0(%rsp)
        fnstcw  4(%rsp)
        movdqu %xmm6,  16(%rsp)
        movdqu %xmm7,  32(%rsp)
        movdqu %xmm8,  48(%rsp)
        movdqu %xmm9,  64(%rsp)
        movdqu %xmm10, 80(%rsp)
        movdqu %xmm11, 96(%rsp)
        movdqu %xmm12, 112(%rsp)
        movdqu %xmm13, 128(%rsp)
        movdqu %xmm14, 144(%rsp)
        movdqu %xmm15, 160(%rsp)
        mov    %rsp, (%rcx)
        mov    (%rdx), %rsp
        ldmxcsr 0(%rsp)
        fldcw   4(%rsp)
        movdqu 16(%rsp),  %xmm6
        movdqu 32(%rsp),  %xmm7
        movdqu 48(%rsp),  %xmm8
        movdqu 64(%rsp),  %xmm9
        movdqu 80(%rsp),  %xmm10
        movdqu 96(%rsp),  %xmm11
        movdqu 112(%rsp), %xmm12
        movdqu 128(%rsp), %xmm13
        movdqu 144(%rsp), %xmm14
        movdqu 160(%rsp), %xmm15
        add    $176, %rsp
        pop    %r15
        pop    %r14
        pop    %r13
        pop    %r12
        pop    %rsi
        pop    %rdi
        pop    %rbx
        pop    %rbp
        pop    %rax
        movq   %rax, %gs:0x10
        pop    %rax
        movq   %rax, %gs:0x08
        ret
    "#,
    options(att_syntax)
);

// SysV i386 (x86-32), non-Windows. Two things differ from x86_64 and
// both matter:
//
//   * **Arguments arrive on the stack**, not in registers: at entry
//     `4(%esp)` is `save_to` and `8(%esp)` is `load_from`, above the
//     return address. They must be read into registers *before* the
//     first push, because every push moves the stack they sit on.
//   * The callee-saved set is only {ebx, esi, edi, ebp}. As on x86_64
//     the MXCSR control bits and the x87 control word are callee-saved
//     (i386 psABI §2.2.1), so the frame carries them too — and unlike
//     x86_64 this target genuinely still has an x87 stack in play.
//
// Saved frame, low → high address, from the saved sp:
//   sp+0   : MXCSR   (4 bytes)
//   sp+4   : x87 CW  (2 bytes, 2 of pad)
//   sp+8   : edi
//   sp+12  : esi
//   sp+16  : ebx     — carries the TrampolineState on first entry
//   sp+20  : ebp
//   sp+24  : return address pushed by the `call`
#[cfg(all(target_arch = "x86", not(windows)))]
global_asm!(
    r#"
    .global _krio_fiber_switch
    .global krio_fiber_switch
    _krio_fiber_switch:
    krio_fiber_switch:
        mov    4(%esp), %eax
        mov    8(%esp), %edx
        push   %ebp
        push   %ebx
        push   %esi
        push   %edi
        sub    $8, %esp
        stmxcsr (%esp)
        fnstcw  4(%esp)
        mov    %esp, (%eax)
        mov    (%edx), %esp
        ldmxcsr (%esp)
        fldcw   4(%esp)
        add    $8, %esp
        pop    %edi
        pop    %esi
        pop    %ebx
        pop    %ebp
        ret
    "#,
    options(att_syntax)
);

// RISC-V RV64GC, LP64D. Callee-saved is {ra, s0-s11} plus {fs0-fs11}
// under the D extension, and s0 doubles as the frame pointer — so it
// sits at a fixed offset a GC walker can find, exactly like x29 on
// AArch64.
//
// `fcsr` rides the frame as well. The psABI does not list it as
// callee-saved, but restoring the *incoming* context's rounding mode
// and exception flags is right under either reading: a context that
// set a rounding mode before yielding should still have it on resume.
// It costs two instructions.
//
// Saved frame, low → high address, from the saved sp:
//   sp+0   : ra            — return address, i.e. the resume point
//   sp+8   : s0 / fp
//   sp+16  : s1            — carries the TrampolineState on first entry
//   sp+24  : s2..s11       (10 × 8)
//   sp+104 : fs0..fs11     (12 × 8)
//   sp+200 : fcsr (4 bytes, 4 of pad)
//   total  : 208, a multiple of 16 as the ABI requires
#[cfg(target_arch = "riscv64")]
global_asm!(
    r#"
    // `global_asm!` does not inherit the target's features, so the
    // assembler starts at base rv64i and rejects every fsd/fld below —
    // even on riscv64gc, whose `g` already includes D. Declare the
    // extension for this block, and pop it so it cannot leak into
    // anything assembled afterwards.
    .option push
    .option arch, +d
    .global krio_fiber_switch
    krio_fiber_switch:
        addi  sp, sp, -208
        sd    ra,  0(sp)
        sd    s0,  8(sp)
        sd    s1,  16(sp)
        sd    s2,  24(sp)
        sd    s3,  32(sp)
        sd    s4,  40(sp)
        sd    s5,  48(sp)
        sd    s6,  56(sp)
        sd    s7,  64(sp)
        sd    s8,  72(sp)
        sd    s9,  80(sp)
        sd    s10, 88(sp)
        sd    s11, 96(sp)
        fsd   fs0,  104(sp)
        fsd   fs1,  112(sp)
        fsd   fs2,  120(sp)
        fsd   fs3,  128(sp)
        fsd   fs4,  136(sp)
        fsd   fs5,  144(sp)
        fsd   fs6,  152(sp)
        fsd   fs7,  160(sp)
        fsd   fs8,  168(sp)
        fsd   fs9,  176(sp)
        fsd   fs10, 184(sp)
        fsd   fs11, 192(sp)
        frcsr t0
        sw    t0, 200(sp)
        sd    sp, 0(a0)
        ld    sp, 0(a1)
        ld    ra,  0(sp)
        ld    s0,  8(sp)
        ld    s1,  16(sp)
        ld    s2,  24(sp)
        ld    s3,  32(sp)
        ld    s4,  40(sp)
        ld    s5,  48(sp)
        ld    s6,  56(sp)
        ld    s7,  64(sp)
        ld    s8,  72(sp)
        ld    s9,  80(sp)
        ld    s10, 88(sp)
        ld    s11, 96(sp)
        fld   fs0,  104(sp)
        fld   fs1,  112(sp)
        fld   fs2,  120(sp)
        fld   fs3,  128(sp)
        fld   fs4,  136(sp)
        fld   fs5,  144(sp)
        fld   fs6,  152(sp)
        fld   fs7,  160(sp)
        fld   fs8,  168(sp)
        fld   fs9,  176(sp)
        fld   fs10, 184(sp)
        fld   fs11, 192(sp)
        lw    t0, 200(sp)
        fscsr t0
        addi  sp, sp, 208
        ret
    .option pop
    "#
);

// AAPCS64, non-Windows. ARM64 Windows deliberately falls through to
// the unsupported arm below — see the module docs.
#[cfg(all(target_arch = "aarch64", not(windows)))]
global_asm!(
    r#"
    .global _krio_fiber_switch
    .global krio_fiber_switch
    _krio_fiber_switch:
    krio_fiber_switch:
        sub  sp, sp, #192
        stp  x19, x20, [sp, #0]
        stp  x21, x22, [sp, #16]
        stp  x23, x24, [sp, #32]
        stp  x25, x26, [sp, #48]
        stp  x27, x28, [sp, #64]
        stp  x29, x30, [sp, #80]
        // AAPCS64 also makes the low 64 bits of v8-v15 callee-saved;
        // extern "C" callers may keep live doubles there across a
        // yield/resume, so they must ride the frame too (d-regs at
        // [96, 160); pad to 176 keeps 16-byte alignment). GP layout
        // above is unchanged so SAVED_FP_OFFSET/SAVED_RET_OFFSET hold.
        stp  d8,  d9,  [sp, #96]
        stp  d10, d11, [sp, #112]
        stp  d12, d13, [sp, #128]
        stp  d14, d15, [sp, #144]
        // AAPCS64 also makes FPCR (rounding mode + exception masks)
        // callee-saved; it lives at [sp, #160] and the frame pads to
        // 192. FPSR is *not* callee-saved, so it is left alone.
        mrs  x9, fpcr
        str  x9, [sp, #160]
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
        ldp  d8,  d9,  [sp, #96]
        ldp  d10, d11, [sp, #112]
        ldp  d12, d13, [sp, #128]
        ldp  d14, d15, [sp, #144]
        ldr  x9, [sp, #160]
        msr  fpcr, x9
        add  sp, sp, #192
        ret
    "#
);

// Unsupported targets: stack-based context switching has no
// implementation. Two groups land here:
//
//   * Architectures with no port at all (wasm32, riscv64, arm, x86-32,
//     powerpc*, s390x, …). The crate still needs to *compile* — it is
//     pulled in transitively on targets that drive concurrency through
//     the stackless `krio-async` path instead of native fibers.
//   * `aarch64-pc-windows-*`. The AAPCS64 register save above would
//     assemble and run there, but ARM64 Windows additionally needs the
//     TEB stack-bounds swap that the MS x64 path does (without it SEH
//     refuses to unwind out of a fiber and the first panic crossing a
//     fiber boundary kills the process with 0xe06d7363), and
//     `super::stack` has no guard-page allocator for it. An honest
//     panic beats a switch that works right up until the first
//     exception. See the module docs for what a real port needs (x18).
//
// `krio_fiber_switch` compiles but panics if actually invoked; the
// layout constants are inert zeros.
#[cfg(not(any(
    target_arch = "x86_64",
    all(target_arch = "x86", not(windows)),
    target_arch = "riscv64",
    all(target_arch = "aarch64", not(windows))
)))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn krio_fiber_switch(_save_to: *mut *mut u8, _load_from: *const *mut u8) {
    panic!(
        "krio-fiber: native context switching is unavailable on this target; \
         drive concurrency through the stackless krio-async path instead"
    );
}

/// Number of bytes [`krio_fiber_switch`] pushes onto the stack
/// during a save. Used by [`super::fiber::prepare_initial_stack`]
/// to lay out the fake saved frame for a brand-new fiber.
#[cfg(all(target_arch = "x86_64", not(windows)))]
pub const SAVED_FRAME_BYTES: usize = 6 * 8 + 16; // rbp, rbx, r12..r15 + MXCSR/x87 CW

#[cfg(all(target_arch = "x86_64", windows))]
pub const SAVED_FRAME_BYTES: usize = 8 * 8 + 10 * 16 + 16 + 16; // GP + xmm + TEB + FP ctl = 256

#[cfg(all(target_arch = "aarch64", not(windows)))]
pub const SAVED_FRAME_BYTES: usize = 192; // 12 GP + 8 FP regs + FPCR + alignment pad

#[cfg(all(target_arch = "x86", not(windows)))]
pub const SAVED_FRAME_BYTES: usize = 4 * 4 + 8; // ebp, ebx, esi, edi + MXCSR/x87 CW

#[cfg(target_arch = "riscv64")]
pub const SAVED_FRAME_BYTES: usize = 208; // ra + 12 s-regs + 12 fs-regs + fcsr + pad

/// Byte offset, from a suspended fiber's [`super::Fiber::saved_sp`],
/// of the saved frame-pointer register (`rbp` on x86_64, `x29` on
/// aarch64). The suspended fiber's stack frame chain starts at
/// `*(saved_sp + SAVED_FP_OFFSET)`. Used by host GCs to walk the
/// fiber's frames when scanning roots across a suspension.
#[cfg(all(target_arch = "x86_64", not(windows)))]
pub const SAVED_FP_OFFSET: usize = 56; // MXCSR/CW pad, r15,r14,r13,r12,rbx then rbp at +56

#[cfg(all(target_arch = "x86_64", windows))]
pub const SAVED_FP_OFFSET: usize = 232; // FP ctl, xmm6..xmm15, r15..r12, rsi, rdi, rbx, rbp

#[cfg(all(target_arch = "aarch64", not(windows)))]
pub const SAVED_FP_OFFSET: usize = 80; // x29 lives at sp+80 (see stp pair)

#[cfg(all(target_arch = "x86", not(windows)))]
pub const SAVED_FP_OFFSET: usize = 20; // MXCSR/CW pad, edi, esi, ebx, then ebp

// s0 is RISC-V's frame pointer, and the switch parks it directly above
// the saved ra.
#[cfg(target_arch = "riscv64")]
pub const SAVED_FP_OFFSET: usize = 8;

/// Byte offset of the saved return address (instruction at which the
/// fiber will resume execution after the next context switch). On
/// x86_64 this is the implicit return address pushed by the `call`
/// to `krio_fiber_switch` and sits just above the saved registers.
/// On aarch64 this is `x30`, saved alongside `x29` in the stp pair.
#[cfg(all(target_arch = "x86_64", not(windows)))]
pub const SAVED_RET_OFFSET: usize = 64; // ret_addr sits above the full 64-byte frame

#[cfg(all(target_arch = "x86_64", windows))]
pub const SAVED_RET_OFFSET: usize = 256; // ret_addr sits above the full 256-byte frame

#[cfg(all(target_arch = "aarch64", not(windows)))]
pub const SAVED_RET_OFFSET: usize = 88; // x30 lives at sp+88 (companion of x29)

#[cfg(all(target_arch = "x86", not(windows)))]
pub const SAVED_RET_OFFSET: usize = 24; // ret_addr sits above the 24-byte frame

// RISC-V keeps the return address in a register, so the switch spills
// `ra` to the very bottom of its frame rather than above it.
#[cfg(target_arch = "riscv64")]
pub const SAVED_RET_OFFSET: usize = 0;

// Inert layout constants for targets without a native context switch — the
// fiber path is never entered at runtime on these (see `krio_fiber_switch`).
#[cfg(not(any(
    target_arch = "x86_64",
    all(target_arch = "x86", not(windows)),
    target_arch = "riscv64",
    all(target_arch = "aarch64", not(windows))
)))]
pub const SAVED_FRAME_BYTES: usize = 0;
#[cfg(not(any(
    target_arch = "x86_64",
    all(target_arch = "x86", not(windows)),
    target_arch = "riscv64",
    all(target_arch = "aarch64", not(windows))
)))]
pub const SAVED_FP_OFFSET: usize = 0;
#[cfg(not(any(
    target_arch = "x86_64",
    all(target_arch = "x86", not(windows)),
    target_arch = "riscv64",
    all(target_arch = "aarch64", not(windows))
)))]
pub const SAVED_RET_OFFSET: usize = 0;
