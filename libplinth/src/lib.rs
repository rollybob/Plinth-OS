//! Syscall wrappers and runtime support for Plinth user processes.
//!
//! This is deliberately NOT a library OS. It is the thinnest possible
//! shim over the kernel interface -- raw syscalls and the C memory
//! intrinsics the compiler expects, nothing more. Allocators, heaps, and
//! every other abstraction belong to the library OSes built on top.
//!
//! ## Syscall numbers (RAX), args in RDI/RSI/RDX
//!
//! | Nr | Name        | Args      | Returns                 |
//! |----|-------------|-----------|-------------------------|
//! |  1 | write       | ptr, len  | len, or SYS_ERR         |
//! |  2 | exit        | code      | (never returns)         |
//! |  3 | frame_alloc | --        | cap slot, or SYS_ERR    |
//! |  4 | frame_map   | slot, va  | 0, or SYS_ERR           |
//! |  5 | frame_free  | slot      | 0, or SYS_ERR           |

#![no_std]

/// Error return shared by all syscalls.
pub const SYS_ERR: u64 = u64::MAX;

/// frame_map only accepts virtual addresses in [MAP_BASE, MAP_END),
/// page-aligned. Mirrors the kernel's user mapping window.
pub const MAP_BASE: u64 = 0x1000_0000;
pub const MAP_END: u64 = 0x2000_0000;

pub const PAGE_SIZE: u64 = 4096;

/// Shared three-argument syscall stub.
///
/// Clobbers: the kernel ABI treats rdi/rsi/rdx as clobbered argument
/// registers, syscall itself clobbers rcx/r11, and the kernel's C
/// dispatcher may clobber the remaining caller-saved registers r8-r10.
/// Every register in that set must be declared here or the compiler will
/// cache values across the syscall in registers the kernel destroys.
#[inline]
fn syscall3(nr: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            inlateout("rdi") a1 => _,
            inlateout("rsi") a2 => _,
            inlateout("rdx") a3 => _,
            out("rcx") _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("r11") _,
            options(nostack, preserves_flags),
        );
    }
    ret
}

/// Write bytes to the kernel console. Returns bytes written or SYS_ERR.
#[inline]
pub fn sys_write(buf: &[u8]) -> u64 {
    syscall3(1, buf.as_ptr() as u64, buf.len() as u64, 0)
}

/// Terminate the calling process.
#[inline]
pub fn sys_exit(code: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") 2u64,
            in("rdi") code,
            options(nostack, noreturn),
        );
    }
}

/// Allocate one physical frame; returns a capability slot or SYS_ERR.
#[inline]
pub fn sys_frame_alloc() -> u64 {
    syscall3(3, 0, 0, 0)
}

/// Map the frame named by `slot` at the page-aligned address `va`
/// (inside [MAP_BASE, MAP_END)). Returns 0 or SYS_ERR.
#[inline]
pub fn sys_frame_map(slot: u64, va: u64) -> u64 {
    syscall3(4, slot, va, 0)
}

/// Unmap (if mapped), revoke, and free the frame named by `slot`.
#[inline]
pub fn sys_frame_free(slot: u64) -> u64 {
    syscall3(5, slot, 0, 0)
}

// ---------------------------------------------------------------------------
// Console helpers (no core::fmt -- it drags in kilobytes of machinery)
// ---------------------------------------------------------------------------

/// Write `val` as 0x-prefixed lowercase hex, minimal digits.
pub fn write_hex(val: u64) {
    let mut buf = [0u8; 18];
    buf[0] = b'0';
    buf[1] = b'x';
    if val == 0 {
        buf[2] = b'0';
        sys_write(&buf[..3]);
        return;
    }
    let digits = (64 - val.leading_zeros() as usize).div_ceil(4);
    let mut i = 0;
    while i < digits {
        let shift = (digits - 1 - i) * 4;
        let d = ((val >> shift) & 0xF) as u8;
        buf[2 + i] = if d < 10 { b'0' + d } else { b'a' + d - 10 };
        i += 1;
    }
    sys_write(&buf[..2 + digits]);
}

/// Write `val` in decimal.
pub fn write_dec(val: u64) {
    let mut buf = [0u8; 20];
    if val == 0 {
        sys_write(b"0");
        return;
    }
    let mut i = buf.len();
    let mut v = val;
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    sys_write(&buf[i..]);
}

// ---------------------------------------------------------------------------
// Memory intrinsics
// ---------------------------------------------------------------------------
// LLVM lowers slice operations (PartialEq, copy_from_slice, etc.) to these
// C symbols. compiler-builtins provides them as weak symbols that can fail
// to resolve in the no_std build-std environment, producing a null function
// pointer and an instruction-fetch fault at RIP=0. Strong definitions here
// guarantee resolution for every crate that links libplinth.
//
// CONSTRAINT: the loop bodies must stay volatile. A plain element-copy loop
// at opt-level 3 + LTO is recognised by LLVM's loop-to-memcpy pass and
// replaced with a call to memcpy -- which IS this function, so the result
// is infinite recursion and a stack overflow in ring 3. Volatile accesses
// cannot be folded into a library call. Do not "simplify" these.

#[no_mangle]
pub unsafe extern "C" fn memcpy(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        dst.add(i).write_volatile(src.add(i).read_volatile());
        i += 1;
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memmove(dst: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    if (dst as usize) < (src as usize) || (dst as usize) >= (src as usize).wrapping_add(n) {
        let mut i = 0;
        while i < n {
            dst.add(i).write_volatile(src.add(i).read_volatile());
            i += 1;
        }
    } else {
        let mut i = n;
        while i > 0 {
            i -= 1;
            dst.add(i).write_volatile(src.add(i).read_volatile());
        }
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memset(dst: *mut u8, val: i32, n: usize) -> *mut u8 {
    let mut i = 0;
    while i < n {
        dst.add(i).write_volatile(val as u8);
        i += 1;
    }
    dst
}

#[no_mangle]
pub unsafe extern "C" fn memcmp(a: *const u8, b: *const u8, n: usize) -> i32 {
    let mut i = 0;
    while i < n {
        let diff = a.add(i).read_volatile() as i32 - b.add(i).read_volatile() as i32;
        if diff != 0 {
            return diff;
        }
        i += 1;
    }
    0
}
