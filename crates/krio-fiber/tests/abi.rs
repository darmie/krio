//! ABI-contract probes for the asm context switch.
//!
//! These are deliberately separate from `basic.rs`: `basic.rs` tests
//! *behaviour* (does the fiber suspend, resume, error), while this file
//! tests the *ABI contract* the switch has to honour — which registers
//! and control words survive a switch, whether the frame stays aligned,
//! whether the guard page traps, and whether a suspended fiber's
//! `[saved_sp, stack_top)` window really contains everything a
//! conservative GC has to see.
//!
//! Several of these are arch-asymmetric by nature. `basic.rs`'s
//! `fp_callee_saved_survive_switch` is the clearest example: it is
//! meaningful on aarch64 (d8-d15) and MS x64 (xmm6-xmm15) but passes
//! *vacuously* on x86_64 SysV, which has no callee-saved FP registers
//! at all. That asymmetry is why the CI matrix has to include an arm64
//! job — the d8-d15 regression this suite now guards against could not
//! have been caught on x86_64 under any amount of luck.

use krio_fiber::{Fiber, FiberState, yield_now};

// ── 1. GP callee-saved register pressure across the switch ────────────
// The integer twin of `fp_callee_saved_survive_switch`. Unlike that one
// this is meaningful on *every* target: rbx/rbp/r12-r15 on x86_64,
// x19-x28 on aarch64. Eight values live across 64 switches on both
// sides forces the allocator into the callee-saved bank.

#[inline(never)]
fn inoise(seed: u64) -> u64 {
    std::hint::black_box(seed).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5DEE_CE66D
}

#[test]
fn gp_callee_saved_survive_switch() {
    let mut fib = Fiber::new(|| {
        let a = inoise(1);
        let b = inoise(2);
        let c = inoise(3);
        let d = inoise(4);
        let e = inoise(5);
        let f = inoise(6);
        let g = inoise(7);
        let h = inoise(8);
        for _ in 0..64 {
            yield_now();
            assert_eq!(a, inoise(1));
            assert_eq!(b, inoise(2));
            assert_eq!(c, inoise(3));
            assert_eq!(d, inoise(4));
            assert_eq!(e, inoise(5));
            assert_eq!(f, inoise(6));
            assert_eq!(g, inoise(7));
            assert_eq!(h, inoise(8));
        }
    });
    let ha = inoise(11);
    let hb = inoise(12);
    let hc = inoise(13);
    let hd = inoise(14);
    let he = inoise(15);
    let hf = inoise(16);
    let hg = inoise(17);
    let hh = inoise(18);
    while !fib.is_done() {
        fib.resume();
        assert_eq!(ha, inoise(11));
        assert_eq!(hb, inoise(12));
        assert_eq!(hc, inoise(13));
        assert_eq!(hd, inoise(14));
        assert_eq!(he, inoise(15));
        assert_eq!(hf, inoise(16));
        assert_eq!(hg, inoise(17));
        assert_eq!(hh, inoise(18));
    }
}

// ── 2. FP control / rounding word must not leak out of a fiber ────────
// SysV AMD64 §3.2.1 makes the MXCSR *control* bits and the x87 control
// word callee-saved; MS x64 says the same; AAPCS64 says the same for
// FPCR. A switch that does not spill them lets a fiber that changes
// rounding mode silently change its host's arithmetic.

#[cfg(target_arch = "x86_64")]
mod fpctl {
    pub fn mxcsr() -> u32 {
        let mut v: u32 = 0;
        // SAFETY: `stmxcsr` writes 4 bytes to the supplied address.
        unsafe { std::arch::asm!("stmxcsr [{}]", in(reg) &raw mut v, options(nostack)) };
        v
    }
    pub fn set_mxcsr(v: u32) {
        // SAFETY: `ldmxcsr` reads 4 bytes from the supplied address.
        unsafe { std::arch::asm!("ldmxcsr [{}]", in(reg) &raw const v, options(nostack)) };
    }
    pub fn x87_cw() -> u16 {
        let mut v: u16 = 0;
        // SAFETY: `fnstcw` writes 2 bytes to the supplied address.
        unsafe { std::arch::asm!("fnstcw [{}]", in(reg) &raw mut v, options(nostack)) };
        v
    }
    pub fn set_x87_cw(v: u16) {
        // SAFETY: `fldcw` reads 2 bytes from the supplied address.
        unsafe { std::arch::asm!("fldcw [{}]", in(reg) &raw const v, options(nostack)) };
    }
    /// MXCSR rounding-control field, bits 14:13.
    pub const MXCSR_RC_TOWARD_ZERO: u32 = 0b11 << 13;
    /// x87 rounding-control field, bits 11:10.
    pub const X87_RC_TOWARD_ZERO: u16 = 0b11 << 10;
}

#[cfg(target_arch = "aarch64")]
mod fpctl {
    pub fn fpcr() -> u64 {
        let v: u64;
        // SAFETY: reading FPCR is unprivileged and has no side effects.
        unsafe { std::arch::asm!("mrs {}, fpcr", out(reg) v, options(nomem, nostack)) };
        v
    }
    pub fn set_fpcr(v: u64) {
        // SAFETY: writing FPCR is unprivileged; the caller restores it.
        unsafe { std::arch::asm!("msr fpcr, {}", in(reg) v, options(nomem, nostack)) };
    }
    /// FPCR.RMode, bits 23:22. 0b11 = round toward zero.
    pub const FPCR_RMODE_TOWARD_ZERO: u64 = 0b11 << 22;
}

#[test]
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn fp_control_word_does_not_leak_from_fiber_to_host() {
    #[cfg(target_arch = "x86_64")]
    {
        use fpctl::*;
        let (mx_before, cw_before) = (mxcsr(), x87_cw());
        let mut fib = Fiber::new(|| {
            set_mxcsr(mxcsr() | MXCSR_RC_TOWARD_ZERO);
            set_x87_cw(x87_cw() | X87_RC_TOWARD_ZERO);
            yield_now();
        });
        fib.resume();
        assert_eq!(fib.state(), FiberState::Suspended);
        let (mx_after, cw_after) = (mxcsr(), x87_cw());
        assert_eq!(
            mx_before, mx_after,
            "MXCSR control bits leaked out of the fiber: {mx_before:#x} -> {mx_after:#x}"
        );
        assert_eq!(
            cw_before, cw_after,
            "x87 control word leaked out of the fiber: {cw_before:#x} -> {cw_after:#x}"
        );
    }
    #[cfg(target_arch = "aarch64")]
    {
        use fpctl::*;
        let before = fpcr();
        let mut fib = Fiber::new(|| {
            set_fpcr(fpcr() | FPCR_RMODE_TOWARD_ZERO);
            yield_now();
        });
        fib.resume();
        assert_eq!(fib.state(), FiberState::Suspended);
        let after = fpcr();
        assert_eq!(
            before, after,
            "FPCR leaked out of the fiber: {before:#x} -> {after:#x}"
        );
    }
}

// ── 3. A fresh fiber must start with FP exceptions MASKED ─────────────
// The initial saved frame is mostly zeroed, and on x86_64 an all-zero
// MXCSR / x87 CW unmasks *every* FP exception — the first denormal or
// inexact result inside a new fiber would then raise SIGFPE. The
// prepare path therefore seeds the ABI defaults (0x1F80 / 0x037F)
// rather than letting those two slots ride along with the zeroing.
// (An all-zero AArch64 FPCR is already the sane default, so aarch64
// only needs the arithmetic half of this check.)
#[test]
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn fresh_fiber_starts_with_fp_exceptions_masked() {
    let mut fib = Fiber::new(|| {
        #[cfg(target_arch = "x86_64")]
        {
            let mx = fpctl::mxcsr();
            // Bits 12:7 are the six SSE exception masks; all must be set.
            assert_eq!(
                (mx >> 7) & 0x3F,
                0x3F,
                "fresh fiber has unmasked SSE exceptions: MXCSR={mx:#x}"
            );
            let cw = fpctl::x87_cw();
            // Bits 5:0 are the six x87 exception masks; all must be set.
            assert_eq!(
                cw & 0x3F,
                0x3F,
                "fresh fiber has unmasked x87 exceptions: CW={cw:#x}"
            );
        }
        // Arithmetic that traps if the masks are wrong: a denormal
        // (underflow + inexact + denormal-operand all at once).
        let tiny = std::hint::black_box(f64::MIN_POSITIVE);
        let denormal = std::hint::black_box(tiny / 8.0e15);
        assert!(denormal >= 0.0);
        let inexact = std::hint::black_box(1.0f64) / std::hint::black_box(3.0f64);
        assert!(inexact.is_finite());
        yield_now();
    });
    fib.resume();
    assert_eq!(fib.state(), FiberState::Suspended);
    while !fib.is_done() {
        fib.resume();
    }
}

// ── 4. Frame alignment at an ABI boundary inside the fiber ────────────
// The trampoline hands `fiber_run` a synthetic frame. If its alignment
// residue differs from a real call's, the first 16-byte-aligned SSE /
// NEON spill inside the fiber faults (movaps #GP on x86_64). Rather
// than hard-code a residue, compare the fiber side against the host
// side measured through the identical call.

#[inline(never)]
fn sp_now() -> usize {
    let sp: usize;
    #[cfg(target_arch = "x86_64")]
    // SAFETY: reads rsp into a register; no memory or flags touched.
    unsafe {
        std::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags))
    };
    #[cfg(target_arch = "aarch64")]
    // SAFETY: reads sp into a register; no memory or flags touched.
    unsafe {
        std::arch::asm!("mov {}, sp", out(reg) sp, options(nomem, nostack, preserves_flags))
    };
    sp
}

#[inline(never)]
fn aligned_simd_work(x: f64) -> f64 {
    #[repr(align(16))]
    struct A16([f64; 2]);
    let mut v = A16([x, x * 2.0]);
    std::hint::black_box(&mut v);
    v.0[0] + v.0[1]
}

#[test]
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn fiber_frame_alignment_matches_host() {
    let host_residue = sp_now() % 16;
    let mut fib = Fiber::new(move || {
        for i in 0..32 {
            // Faults outright if the frame is misaligned on x86_64.
            let r = aligned_simd_work(i as f64);
            assert!(r.is_finite());
            let residue = sp_now() % 16;
            assert_eq!(
                residue, host_residue,
                "fiber frame alignment ({residue}) differs from host ({host_residue})"
            );
            yield_now();
        }
    });
    while !fib.is_done() {
        fib.resume();
    }
}

// ── 5. GC contract: saved_sp lands inside the registered stack range ──
#[test]
fn saved_sp_window_lies_inside_registered_stack_range() {
    let mut fib = Fiber::new(|| {
        yield_now();
        yield_now();
    });
    let (base, len) = fib.stack_range();
    let lo = base as usize;
    let hi = lo + len;
    fib.resume();
    let sp = fib.saved_sp() as usize;
    assert!(
        sp >= lo && sp < hi,
        "saved_sp {sp:#x} outside stack range [{lo:#x},{hi:#x})"
    );
    eprintln!("scan window = {} bytes", hi - sp);
    let fp = fib.saved_fp().expect("suspended fiber has a saved fp");
    assert!(!fp.is_null(), "saved_fp is null (frame pointers omitted?)");
    let ret = fib.saved_ret().expect("suspended fiber has a saved ret");
    assert!(!ret.is_null());
    fib.resume();
    let sp2 = fib.saved_sp() as usize;
    assert!(sp2 >= lo && sp2 < hi);
    while !fib.is_done() {
        fib.resume();
    }
}

// ── 6. GC contract: a pointer live only in a register must be scannable ─
// This is the property ash's conservative GC leans on. A heap pointer
// held across a yield — including one the compiler parked in a
// callee-saved register — has to appear somewhere in
// `[saved_sp, stack_top)`, because the switch spills the callee-saved
// bank onto the *outgoing* stack inside exactly that window.
#[test]
fn live_pointer_across_yield_is_findable_in_scan_window() {
    let boxed = Box::new(0xDEAD_BEEF_u64);
    let addr = &*boxed as *const u64 as usize;
    let mut fib = Fiber::new(move || {
        let p = &*boxed as *const u64;
        let q = std::hint::black_box(p);
        yield_now();
        // SAFETY: `boxed` is moved into the closure and outlives `q`.
        assert_eq!(unsafe { *q }, 0xDEAD_BEEF);
        std::hint::black_box(&boxed);
    });
    fib.resume();
    assert_eq!(fib.state(), FiberState::Suspended);
    let (base, len) = fib.stack_range();
    let sp = fib.saved_sp() as usize;
    let top = base as usize + len;
    let mut found = false;
    let mut w = (sp + 7) & !7;
    while w + 8 <= top {
        // SAFETY: `w` walks word-aligned addresses strictly inside the
        // fiber's own live, mapped stack region.
        if unsafe { *(w as *const usize) } == addr {
            found = true;
            break;
        }
        w += 8;
    }
    assert!(
        found,
        "live pointer {addr:#x} not present anywhere in the GC scan window \
         [{sp:#x},{top:#x}) — conservative scanning would miss it"
    );
    while !fib.is_done() {
        fib.resume();
    }
}

// ── 7. Guard page must trap on stack overflow ─────────────────────────
// Runs in a child process, because a successful test *is* a fatal
// signal. The child re-executes this same test binary with an env
// marker set, recurses inside a 64 KB fiber until it runs off the
// bottom, and is expected to die on the PROT_NONE guard page:
// SIGSEGV on Linux, SIGBUS on macOS.
//
// Deliberately unix-only: `krio_fiber::stack` allocates a plain
// `Box<[u8]>` on non-unix targets, so **Windows has no guard page at
// all** and a fiber stack overflow there corrupts adjacent heap
// instead of trapping. See the `#[ignore]`d twin below.

const GUARD_PROBE_ENV: &str = "KRIO_FIBER_GUARD_PAGE_PROBE_CHILD";

#[cfg(unix)]
#[inline(never)]
fn eat_stack(depth: u64, pad: &mut [u8; 1024]) -> u64 {
    pad[0] = depth as u8;
    std::hint::black_box(&pad);
    if depth > 100_000 {
        return depth;
    }
    let mut next = [0u8; 1024];
    eat_stack(depth + 1, &mut next) + pad[0] as u64
}

#[cfg(unix)]
#[test]
fn guard_page_traps_on_stack_overflow() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::Command;

    if std::env::var_os(GUARD_PROBE_ENV).is_some() {
        // Child role: dive until the guard page stops us.
        let mut fib = Fiber::with_stack_size(64 * 1024, || {
            let mut pad = [0u8; 1024];
            let depth = eat_stack(0, &mut pad);
            println!("NO_TRAP: recursion bottomed out at depth {depth}");
        });
        fib.resume();
        println!("NO_TRAP: fiber returned in state {:?}", fib.state());
        return;
    }

    let exe = std::env::current_exe().expect("current_exe");
    let out = Command::new(exe)
        .args([
            "guard_page_traps_on_stack_overflow",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(GUARD_PROBE_ENV, "1")
        .output()
        .expect("spawn guard-page child probe");

    // SIGSEGV is 11 everywhere; SIGBUS is 7 on Linux and 10 on Darwin.
    // Darwin reports a guard-page hit as SIGBUS.
    let signal = out.status.signal();
    assert!(
        matches!(signal, Some(7) | Some(10) | Some(11)),
        "fiber stack overflow did not trap on the guard page: \
         status={:?} signal={:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        signal,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

/// Documents, in CI output, that non-unix targets have **no guard
/// page**: `krio_fiber::stack` falls back to `Box<[u8]>` there, so a
/// fiber stack overflow silently corrupts adjacent heap rather than
/// trapping. Remove the `#[ignore]` once Windows gets a
/// `VirtualAlloc` + `PAGE_NOACCESS` stack allocator.
#[cfg(not(unix))]
#[test]
#[ignore = "no guard page on non-unix targets: fiber stacks are plain Box<[u8]>"]
fn guard_page_traps_on_stack_overflow() {
    unreachable!("guard-page probe is unix-only for now");
}
