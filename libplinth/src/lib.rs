//! Syscall wrappers and runtime support for Plinth user processes.
//!
//! This is deliberately NOT a library OS. It is the thinnest possible
//! shim over the kernel interface -- raw syscalls and the C memory
//! intrinsics the compiler expects, nothing more. Allocators, heaps, and
//! every other abstraction belong to the library OSes built on top.
//!
//! ## Syscall numbers (RAX), args in RDI/RSI/RDX
//!
//! | Nr | Name        | Args         | Returns                  |
//! |----|-------------|--------------|--------------------------|
//! |  1 | write       | ptr, len     | len, or SYS_ERR          |
//! |  2 | exit        | code         | (never returns)          |
//! |  3 | frame_alloc | --           | cap slot, or SYS_ERR     |
//! |  4 | frame_map   | slot, va     | 0, or SYS_ERR            |
//! |  5 | frame_free  | slot         | 0, or SYS_ERR            |
//! |  6 | cpu_charge  | slot, amount | remaining, or terminates |
//! |  7 | fault_reg   | entry, stack | 0, or SYS_ERR            |
//! |  8 | fault_return| --           | (resumes faulting insn)  |
//! |  9 | spawn       | child_id, slot| child exit code, or ERR |
//! | 10 | block_read  | rng,frm,sec,cnt| BLK_OK, or a BLK_E_* code |

#![no_std]

/// Error return shared by all syscalls.
pub const SYS_ERR: u64 = u64::MAX;

/// frame_map only accepts virtual addresses in [MAP_BASE, MAP_END),
/// page-aligned. Mirrors the kernel's user mapping window.
pub const MAP_BASE: u64 = 0x1000_0000;
pub const MAP_END: u64 = 0x2000_0000;

pub const PAGE_SIZE: u64 = 4096;

/// The kernel mints each process a CPU-time capability at spawn, and it
/// always lands in this slot -- a well-known initial capability, the way
/// fd 0 is on Unix. Pass it to sys_cpu_charge.
pub const CPU_CAP_SLOT: u64 = 0;

/// A capability a parent transfers into a child via sys_spawn lands here,
/// in the child's table -- the next slot after the CPU budget. A spawned
/// child reads its inherited capability from this slot.
pub const GRANT_SLOT: u64 = 1;

/// An endpoint capability the kernel grants a scheduler-launched IPC process
/// lands here too -- the next mint after the CPU budget. Pass it to
/// sys_send / sys_recv. (Same slot as GRANT_SLOT: a process gets one or the
/// other depending on how it was launched.)
pub const ENDPOINT_SLOT: u64 = 1;

/// A BlockRange capability the kernel grants a scheduler-launched block process
/// lands here too -- the same first-grant slot as ENDPOINT_SLOT/GRANT_SLOT.
/// Pass it to sys_block_read.
pub const BLOCK_SLOT: u64 = 1;

/// Demand-paged window. A not-present access here, once the process has
/// registered a fault handler (sys_fault_reg), is delivered to that handler
/// instead of killing the process. Inside [MAP_BASE, MAP_END), so the
/// handler can satisfy the fault with the ordinary sys_frame_map.
pub const LAZY_BASE: u64 = 0x1800_0000;
pub const LAZY_END: u64 = 0x1900_0000;

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

/// Four-argument syscall stub. Adds a fourth argument in r8 -- the System V C
/// ABI's fifth register, which the kernel's entry stub leaves untouched, so the
/// dispatcher receives it directly. Same clobber discipline as syscall3 (r8 is
/// an input here, so it is inlateout rather than a plain clobber).
#[inline]
fn syscall4(nr: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> u64 {
    let ret: u64;
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") nr => ret,
            inlateout("rdi") a1 => _,
            inlateout("rsi") a2 => _,
            inlateout("rdx") a3 => _,
            inlateout("r8") a4 => _,
            out("rcx") _,
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

/// Charge `amount` CPU ticks against the CpuTime capability at `slot`
/// (normally CPU_CAP_SLOT). Returns the remaining budget. There is no
/// error return for overdraw: if the process charges more than it holds,
/// the kernel terminates it and this call never returns.
#[inline]
pub fn sys_cpu_charge(slot: u64, amount: u64) -> u64 {
    syscall3(6, slot, amount, 0)
}

/// Launch the embedded child binary `child_id` as an independent, concurrently
/// scheduled process, and return a handle to wait on its result. The kernel
/// sets up a result channel: the child receives a send capability to it (at
/// ENDPOINT_SLOT) and this process receives the matching receive capability --
/// the returned handle. `sys_recv(handle)` collects the child's result (and is
/// the wait). `transfer_slot` optionally moves one capability into the child
/// (landing at GRANT_SLOT); pass `NO_CAP` for none. Returns the handle, or
/// SYS_ERR. Non-blocking -- the child runs alongside the caller.
#[inline]
pub fn sys_spawn(child_id: u64, transfer_slot: u64) -> u64 {
    syscall3(9, child_id, transfer_slot, 0)
}

/// Register `entry` as this process's page-fault handler, to run on the
/// stack ending at `stack_top`. A not-present fault in [LAZY_BASE, LAZY_END)
/// then upcalls `entry` with the fault address in the first argument,
/// instead of terminating. Returns 0, or SYS_ERR if either value is zero.
#[inline]
pub fn sys_fault_reg(entry: u64, stack_top: u64) -> u64 {
    syscall3(7, entry, stack_top, 0)
}

/// Return from a fault handler: resume the instruction that faulted. Does
/// not return to the handler on success. If called outside a fault (or it
/// otherwise fails), it falls through; the safety net exits the process.
#[inline]
pub fn sys_fault_return() -> ! {
    syscall3(8, 0, 0, 0);
    // Reached only if the kernel refused the resume -- treat as fatal.
    sys_exit(120)
}

// ---------------------------------------------------------------------------
// Block storage
// ---------------------------------------------------------------------------

/// block_read status, returned in rax. The data lands in the caller's frame, so
/// status is its own word (the same status/payload split as IPC): no read-back
/// byte can be mistaken for an error. BLK_OK means the sectors were read.
pub const BLK_OK: u64 = 0;
/// count is zero, or count*512 would overflow the I/O frame.
pub const BLK_E_BADARG: u64 = 1;
/// The request falls outside the holder's BlockRange (the multiplexing guard).
pub const BLK_E_RANGE: u64 = 2;
/// Bad slot, wrong object kind, or a missing right on the range or frame cap.
pub const BLK_E_RIGHTS: u64 = 3;
/// The device reported an error or is not initialised.
pub const BLK_E_DEV: u64 = 4;

/// Read `count` 512-byte sectors -- starting `sector_off` sectors into the
/// BlockRange capability at `range_slot` -- into the frame named by
/// `frame_slot`. The device DMAs into the frame, so map it (sys_frame_map) to
/// read the bytes; the frame cap must carry the write right (frame_alloc grants
/// it). Sectors are named relative to the range, never absolutely. Returns
/// BLK_OK, or a BLK_E_* status.
#[inline]
pub fn sys_block_read(range_slot: u64, frame_slot: u64, sector_off: u64, count: u64) -> u64 {
    syscall4(10, range_slot, frame_slot, sector_off, count)
}

// ---------------------------------------------------------------------------
// IPC (synchronous endpoints)
// ---------------------------------------------------------------------------
// The blocking IPC operations enter through a software-interrupt gate
// (`int 0x80`), NOT `syscall`. A blocking call must be suspendable and
// resumable with a return value, and the kernel resumes a process by
// restoring a full register trap frame -- which an interrupt entry saves but
// the syscall fast path does not. The op selector goes in rax, args in
// rdi/rsi; the result comes back as status in rax, payload in rsi, and any
// transferred-cap slot in rdx (ABI v2 -- status is split from the payload so a
// peer-controlled word can never be mistaken for an error).

const IPC_SEND: u64 = 0;
const IPC_RECV: u64 = 1;
const IPC_CALL: u64 = 2;
const IPC_REPLY: u64 = 3;

/// No-capability sentinel: pass to a plain `send` (no cap), and the value
/// `recv` reports for the landing slot when no capability was transferred.
/// A real slot is a small index, so `u64::MAX` is unambiguous.
pub const NO_CAP: u64 = u64::MAX;

/// IPC status, returned in rax separately from the message payload (rsi) and
/// any transferred-cap slot (rdx). A peer controls the whole payload word, so
/// the status MUST be its own field: no value -- not even `u64::MAX` -- can be
/// mistaken for an error or a dead peer (ABI v2). `recv`/`call` return
/// `(status, ...)`; a non-`IPC_OK` status means no message was delivered.
/// `IPC_PEER_DIED` is delivered by the kernel's death-time reaping.
pub const IPC_OK: u64 = 0;
pub const IPC_ERR: u64 = 1;
pub const IPC_PEER_DIED: u64 = 2;

/// Send the one-word message `msg` on the endpoint capability at `ep_slot`,
/// blocking until a receiver takes it. Returns `IPC_OK`, or `IPC_ERR` for a
/// bad slot or missing send right.
#[inline]
pub fn sys_send(ep_slot: u64, msg: u64) -> u64 {
    sys_send_cap(ep_slot, msg, NO_CAP)
}

/// Like `sys_send`, but also transfer the capability at `cap_slot` to the
/// receiver (moving it out of this process's table -- and, if it is a mapped
/// frame, unmapping it here). The receiver learns the cap's new slot from
/// `sys_recv_cap`. Pass `NO_CAP` for a word-only send.
#[inline]
pub fn sys_send_cap(ep_slot: u64, msg: u64, cap_slot: u64) -> u64 {
    let ret: u64;
    // SAFETY: int 0x80 is the kernel's IPC gate; its handler saves and
    // restores every register except rax (the result), so the only state
    // this clobbers is rax. The extra clobbers below are conservative.
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") IPC_SEND => ret,
            in("rdi") ep_slot,
            in("rsi") msg,
            in("rdx") cap_slot,
            out("rcx") _, out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Receive a one-word message from the endpoint capability at `ep_slot`,
/// blocking until a sender arrives. Returns `(status, msg)`: `status` is
/// `IPC_OK` on a real message (or `IPC_PEER_DIED` / `IPC_ERR`), and `msg` is
/// meaningful only when `status == IPC_OK`. Any capability the sender
/// transferred lands in this process's table but its slot is dropped -- use
/// `sys_recv_cap` when you expect a capability.
#[inline]
pub fn sys_recv(ep_slot: u64) -> (u64, u64) {
    let (status, msg, _cap_slot) = sys_recv_cap(ep_slot);
    (status, msg)
}

/// Receive a message and any transferred capability. Returns `(status, msg,
/// cap_slot)`: `status` (rax) is `IPC_OK` / `IPC_PEER_DIED` / `IPC_ERR`, `msg`
/// (rsi) is the message word, and `cap_slot` (rdx) is where a transferred
/// capability landed in this process's table, or `NO_CAP` if none was sent.
/// `msg` and `cap_slot` are meaningful only when `status == IPC_OK`.
#[inline]
pub fn sys_recv_cap(ep_slot: u64) -> (u64, u64, u64) {
    let status: u64;
    let msg: u64;
    let cap_slot: u64;
    // SAFETY: as sys_send_cap. recv returns status in rax, the message in rsi,
    // and the transferred-cap landing slot in rdx (ABI v2).
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") IPC_RECV => status,
            in("rdi") ep_slot,
            out("rsi") msg,
            out("rdx") cap_slot,
            out("rcx") _, out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            options(nostack),
        );
    }
    (status, msg, cap_slot)
}

/// Send a request and block for a reply (RPC). Sends `req` on the endpoint at
/// `ep_slot` and returns `(status, reply)`: `status` (rax) is `IPC_OK` /
/// `IPC_PEER_DIED` / `IPC_ERR`, and `reply` (rsi) is the server's reply word,
/// meaningful only when `status == IPC_OK`. The kernel mints the server a
/// one-shot reply capability naming this caller; the server answers with
/// `sys_reply`.
#[inline]
pub fn sys_call(ep_slot: u64, req: u64) -> (u64, u64) {
    let status: u64;
    let reply: u64;
    // SAFETY: as sys_send_cap. rsi carries the request in and the reply out.
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") IPC_CALL => status,
            in("rdi") ep_slot,
            inlateout("rsi") req => reply,
            out("rdx") _, out("rcx") _, out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            options(nostack),
        );
    }
    (status, reply)
}

/// Reply to the caller named by the one-shot reply capability at `reply_slot`
/// (which `sys_recv_cap` returned when it received a `call`), delivering `msg`
/// as the caller's `sys_call` result. Consumes the capability. Returns
/// `IPC_OK`, or `IPC_ERR` if the slot is not a live reply capability.
#[inline]
pub fn sys_reply(reply_slot: u64, msg: u64) -> u64 {
    let ret: u64;
    // SAFETY: as sys_send_cap.
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") IPC_REPLY => ret,
            in("rdi") reply_slot,
            in("rsi") msg,
            out("rdx") _, out("rcx") _, out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Spawn child `child_id` and block until it reports a result -- the common
/// "launch a worker and collect its answer" pattern, expressed as a libOS-level
/// helper over the raw mechanism (`spawn` returns a handle; the wait is a `recv`
/// on it). Returns `(status, result)`:
/// - `(IPC_OK, value)` -- the child sent `value` and the wait collected it;
/// - `(IPC_PEER_DIED, _)` -- the child died before sending; the kernel's
///   death-time reaping woke the wait instead of leaving it blocked forever;
/// - `(IPC_ERR, _)` -- the spawn itself failed.
///
/// `transfer_slot` moves one capability into the child (or `NO_CAP` for none).
#[inline]
pub fn spawn_and_wait(child_id: u64, transfer_slot: u64) -> (u64, u64) {
    let handle = sys_spawn(child_id, transfer_slot);
    if handle == SYS_ERR {
        return (IPC_ERR, 0);
    }
    sys_recv(handle)
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
