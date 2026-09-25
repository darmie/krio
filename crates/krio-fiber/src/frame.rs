//! The first frame of a fresh stack: laid out so that switching into it
//! with [`crate::arch::krio_fiber_switch`] "returns" into a trampoline,
//! which finds `state` in a callee-saved register. Shared by `Fiber` and
//! the allocation-free [`crate::raw`] layer, so both match the switch's
//! saved-frame layout.

#[allow(unused_imports)]
use crate::arch::{SAVED_FRAME_BYTES, SAVED_RET_OFFSET};

/// Default MXCSR: all six SSE exceptions masked, round-to-nearest,
/// flush-to-zero off. This is what the SysV process startup state and
/// the CRT both install. **Must not be left as zero** — a zero MXCSR
/// unmasks every SSE exception, so the first denormal or inexact
/// result inside a brand-new fiber would raise SIGFPE.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
const DEFAULT_MXCSR: u32 = 0x1F80;

/// Default x87 control word: extended precision, round-to-nearest,
/// all six x87 exceptions masked. Same reasoning as [`DEFAULT_MXCSR`]
/// — zero here would unmask the x87 exception set.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
const DEFAULT_X87_CW: u16 = 0x037F;

#[cfg(all(target_arch = "x86_64", not(windows)))]
pub(crate) unsafe fn initial_frame(top: *mut u8, state: usize, trampoline: usize) -> *mut u8 {
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
        (sp.add(40) as *mut usize).write(state);
        // ret_addr slot — trampoline entry point.
        (sp.add(SAVED_RET_OFFSET) as *mut usize).write(trampoline);
    }
    sp
}

#[cfg(all(target_arch = "x86_64", windows))]
pub(crate) unsafe fn initial_frame(
    top: *mut u8,
    stack_limit: *mut u8,
    state: usize,
    trampoline: usize,
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
        (sp.add(200) as *mut usize).write(state);
        // TEB.StackLimit: the low end of the fiber's usable stack.
        (sp.add(240) as *mut usize).write(stack_limit as usize);
        // TEB.StackBase: the high end (= the aligned `top`).
        (sp.add(248) as *mut usize).write(top as usize);
        // ret_addr slot — trampoline entry point.
        (sp.add(SAVED_RET_OFFSET) as *mut usize).write(trampoline);
    }
    sp
}

#[cfg(all(target_arch = "aarch64", windows))]
pub(crate) unsafe fn initial_frame(
    top: *mut u8,
    stack_limit: *mut u8,
    state: usize,
    trampoline: usize,
) -> *mut u8 {
    // As the non-Windows twin below, plus the TEB stack bounds the
    // switch swaps on every transition. They occupy padding the other
    // target leaves empty, so the frame and every published offset are
    // identical on both.
    //
    // Seeding these is what lets SEH unwind *out* of the fiber: until
    // the first switch installs them, the runtime would see an `sp`
    // outside the thread's registered stack and refuse to walk.
    let sp = unsafe { top.sub(SAVED_FRAME_BYTES) };
    unsafe {
        core::ptr::write_bytes(sp, 0, SAVED_FRAME_BYTES);
        // x19 carries the trampoline state.
        (sp as *mut usize).write(state);
        // x30 is the address the first switch-in returns to.
        (sp.add(SAVED_RET_OFFSET) as *mut usize).write(trampoline);
        // TEB.StackLimit — the low end of the fiber's usable stack.
        (sp.add(168) as *mut usize).write(stack_limit as usize);
        // TEB.StackBase — the high end, which is the aligned `top`.
        (sp.add(176) as *mut usize).write(top as usize);
    }
    sp
}

#[cfg(all(target_arch = "aarch64", not(windows)))]
pub(crate) unsafe fn initial_frame(top: *mut u8, state: usize, trampoline: usize) -> *mut u8 {
    // The switch's restore sequence wants the stack (low to high) to
    // look like the saved frame produced by the asm save:
    //   [x19][x20][x21][x22][x23][x24][x25][x26][x27][x28][x29][x30]
    //   [d8..d15][fpcr][pad]
    // x30 is the return address — set it to the trampoline.
    // Stash `state` in x19 so the trampoline can recover it.
    let sp = unsafe { top.sub(SAVED_FRAME_BYTES) };
    unsafe {
        // x19 at offset 0
        (sp as *mut usize).write(state);
        // x20..x29 zeroed (offsets 8..88)
        for i in 1..11 {
            (sp.add(i * 8) as *mut usize).write(0);
        }
        // x30 (return address) at offset 88
        (sp.add(88) as *mut usize).write(trampoline);
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

#[cfg(all(target_arch = "x86", not(windows)))]
pub(crate) unsafe fn initial_frame(top: *mut u8, state: usize, trampoline: usize) -> *mut u8 {
    // i386 SysV saved-frame layout (low → high), mirroring the save
    // path in `crate::arch`:
    //   sp+0   : MXCSR            (4 bytes)
    //   sp+4   : x87 control word (2 bytes, 2 of pad)
    //   sp+8   : edi, esi, ebx, ebp
    //   sp+24  : trampoline_addr (the saved return addr)
    //
    // `state` rides in the ebx slot (sp+16) — callee-saved, so the
    // trampoline finds it there on first entry.
    //
    // Alignment: i386 SysV wants esp 16-byte aligned at a call site, so
    // a callee entered by `call` sees `esp % 16 == 12`. Entering by
    // `ret` off sp+24 leaves esp at sp+28, so sp = top-32 puts that at
    // top-4 — the same residue, since `top` is 16-aligned.
    let sp = unsafe { top.sub(SAVED_FRAME_BYTES + 8) };
    unsafe {
        core::ptr::write_bytes(sp, 0, SAVED_FRAME_BYTES);
        // FP control state, seeded with the ABI defaults rather than
        // zero — see DEFAULT_MXCSR. This target still has a live x87
        // stack, so the control word matters more here than on x86_64.
        (sp as *mut u32).write(DEFAULT_MXCSR);
        (sp.add(4) as *mut u16).write(DEFAULT_X87_CW);
        // Trampoline state for ebx.
        (sp.add(16) as *mut usize).write(state);
        (sp.add(SAVED_RET_OFFSET) as *mut usize).write(trampoline);
    }
    sp
}

#[cfg(target_arch = "riscv64")]
pub(crate) unsafe fn initial_frame(top: *mut u8, state: usize, trampoline: usize) -> *mut u8 {
    // RV64 saved-frame layout (low → high), mirroring `crate::arch`:
    //   sp+0   : ra — the resume point, so the trampoline goes here
    //   sp+8   : s0 / fp
    //   sp+16  : s1 — carries `state` into the trampoline
    //   sp+24  : s2..s11
    //   sp+104 : fs0..fs11
    //   sp+200 : fcsr
    //
    // No return address is pushed on RISC-V — `ret` jumps through `ra`
    // — so unlike x86 there is no residue to correct for: `top` is
    // 16-aligned and the 208-byte frame keeps it that way.
    //
    // Zeroing the fcsr slot is the right default here: round-to-nearest
    // with no exception flags raised, the same reasoning as AArch64's
    // all-zero FPCR rather than x86's MXCSR.
    let sp = unsafe { top.sub(SAVED_FRAME_BYTES) };
    unsafe {
        core::ptr::write_bytes(sp, 0, SAVED_FRAME_BYTES);
        (sp.add(SAVED_RET_OFFSET) as *mut usize).write(trampoline);
        (sp.add(16) as *mut usize).write(state);
    }
    sp
}

// Unsupported targets (wasm32, aarch64-windows, …): no native
// stack to lay out. Compiles so the crate builds where it is pulled in
// transitively. Note this panics from `Fiber::with_stack_size`, i.e.
// `Fiber::new` itself fails loudly on these targets rather than handing
// back a fiber that dies later.
#[cfg(not(any(
    target_arch = "x86_64",
    all(target_arch = "x86", not(windows)),
    target_arch = "riscv64",
    target_arch = "aarch64"
)))]
pub(crate) unsafe fn initial_frame(_top: *mut u8, _state: usize, _trampoline: usize) -> *mut u8 {
    panic!("krio-fiber: native fibers are unavailable on this target");
}
