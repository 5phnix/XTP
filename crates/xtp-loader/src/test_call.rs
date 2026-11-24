//! Calls code from a known place, for tests of what that code sees as its
//! caller (stubs, fast-fail reports).

/// Calls `f(args[0], args[1], args[2])` with the Windows x64 convention, or
/// skips the call when `go` is false. Returns `f`'s result (0 when skipped)
/// and the address the call returns to, which is the same either way: a
/// test learns it first, then calls code that never returns.
///
/// # Safety
/// `f` is code that takes these arguments and, if it returns, keeps the
/// convention's non-volatile registers.
pub(crate) unsafe fn call(f: usize, args: [u64; 3], go: bool) -> (u64, usize) {
    let result: u64;
    let back: usize;
    // SAFETY: the stack is aligned for a call on entry (no `nostack`), the
    // 32 bytes below it are the callee's home space, and `clobber_abi`
    // covers what the callee may change; the caller vouches for `f`.
    unsafe {
        std::arch::asm!(
            "test {go}, {go}",
            "jnz 4f",
            "xor eax, eax",
            "jmp 3f",
            "4:",
            "sub rsp, 32",
            "call {f}",
            "2:",
            "add rsp, 32",
            "3:",
            "lea r9, [rip + 2b]",
            f = in(reg) f,
            go = in(reg) u64::from(go),
            lateout("r9") back,
            in("rcx") args[0],
            in("rdx") args[1],
            in("r8") args[2],
            lateout("rax") result,
            clobber_abi("system"),
        );
    }
    (result, back)
}
