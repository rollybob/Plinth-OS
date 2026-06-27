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
//! | 11 | spawn_buf   | buf_va, len, slot| wait handle, or SYS_ERR |
//! | 12 | ring_register | sq_slot, cq_slot, entries | ring cap slot, or ERR |
//! | 13 | ring_submit | ring         | count posted, or SYS_ERR |
//! | 14 | fb_map      | slot, va, info_ptr | 0, or SYS_ERR       |
//!
//! Block I/O is the async-ring ABI (nr 12/13 + `ring_wait` on the `int 0x80`
//! gate, op 6); `sys_block_read` is a single-in-flight shim over it. The old
//! block_read syscall (nr 10, then `int 0x80` op 5) was retired in ABI v2.4.

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

/// An EventSource capability the kernel grants a scheduler-launched input
/// process lands here too -- the same first-grant slot as the others. Pass it
/// to sys_event_recv.
pub const EVENT_SOURCE_SLOT: u64 = 1;

/// A Framebuffer capability the kernel grants a scheduler-launched graphics
/// process lands here too -- the same first-grant slot as the others. Pass it
/// to sys_fb_map (or libgfx's Framebuffer::map).
pub const FB_SLOT: u64 = 1;

/// Pixel-format codes the kernel writes into the FbInfo `format` field at
/// sys_fb_map time (mirrors the kernel's framebuffer.rs FMT_*). A graphics
/// library OS uses these to pick the channel order; the kernel never touches a
/// pixel itself.
pub const FB_FMT_RGB: u32 = 0;
pub const FB_FMT_BGR: u32 = 1;
pub const FB_FMT_U8: u32 = 2;
pub const FB_FMT_OTHER: u32 = 3;

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

/// Map the framebuffer named by the capability at `slot` into this address space
/// at the page-aligned `va` (inside [MAP_BASE, MAP_END), with room for the whole
/// region), and have the kernel write the geometry to `info_ptr`: five u32s in
/// order -- width, height, stride (pixels/row), bytes_per_pixel, format (FB_FMT_*).
/// Returns 0, or SYS_ERR (bad slot, not a Framebuffer capability, missing
/// RIGHT_MAP, or `va`/geometry out of range). `libgfx::Framebuffer::map` wraps
/// this with a typed FbInfo; most callers use that rather than calling here.
#[inline]
pub fn sys_fb_map(slot: u64, va: u64, info_ptr: u64) -> u64 {
    syscall3(14, slot, va, info_ptr)
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

/// Launch a child from an ELF image the caller holds in its own memory, rather
/// than from the kernel's embedded table. `buf` must be a page-aligned,
/// contiguous, mapped buffer (e.g. frames the caller allocated and mapped from
/// MAP_BASE) holding the whole ELF; this is how a library OS launches a program
/// it read from disk. Returns a wait handle (like `sys_spawn`), or SYS_ERR if
/// the buffer is unmapped/out of range/too large or the spawn failed.
/// `transfer_slot` moves one capability into the child (or NO_CAP for none).
/// Non-blocking; collect the result with `sys_recv(handle)`.
#[inline]
pub fn sys_spawn_from_buffer(buf: &[u8], transfer_slot: u64) -> u64 {
    syscall3(11, buf.as_ptr() as u64, buf.len() as u64, transfer_slot)
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

// --- Async completion rings (ABI v2.4) --------------------------------------
//
// The block path is now the ring ABI (Design/async_rings.md). The kernel ships
// the ring mechanism; this is a *reference* single-in-flight use of it -- the
// `sys_block_read` shim below. A real library OS may build a many-in-flight
// async executor over the same three calls instead.
//
// ring_register / ring_submit are non-blocking and ride the `syscall` fast path
// (nr 12/13). ring_wait BLOCKS until the ring's CQ is non-empty, so it is on the
// `int 0x80` gate (op 6), like the IPC ops -- a blocking call needs the gate's
// resumable trap frame.

const RING_WAIT: u64 = 6;

/// ring_register(sq_slot, cq_slot, entries): bind two caller-owned frames (both
/// mapped, read+write) as an SQ/CQ pair and return a ring capability slot, or
/// SYS_ERR. `entries` must be a power of two that fits one frame (<= 64).
#[inline]
pub fn sys_ring_register(sq_slot: u64, cq_slot: u64, entries: u64) -> u64 {
    syscall3(12, sq_slot, cq_slot, entries)
}

/// ring_submit(ring): the doorbell -- drain the SQ and post each request to the
/// device. Returns the number of entries consumed (posted, or completed with an
/// error status), which may be fewer than queued under backpressure; resubmit
/// the remainder after reaping. SYS_ERR on a bad ring handle. Non-blocking.
#[inline]
pub fn sys_ring_submit(ring: u64) -> u64 {
    syscall3(13, ring, 0, 0)
}

/// ring_wait(ring): block until the ring's CQ has at least one unreaped
/// completion, then return 0 (the caller reaps from the CQ in memory). SYS_ERR
/// on a bad ring handle.
#[inline]
pub fn sys_ring_wait(ring: u64) -> u64 {
    let ret: u64;
    // SAFETY: int 0x80 is the kernel's blocking-call gate; its handler saves and
    // restores every register except the result (rax). The wait may block until
    // the virtio completion IRQ posts into this ring's CQ and wakes the process.
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inlateout("rax") RING_WAIT => ret,
            in("rdi") ring,
            out("rsi") _, out("rdx") _, out("rcx") _,
            out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            options(nostack),
        );
    }
    ret
}

// --- Block storage: the single-in-flight ring shim --------------------------

// SQ/CQ entry/header layout (Design/async_rings.md s4), as byte offsets.
const RING_HDR_HEAD: u64 = 0;
const RING_HDR_TAIL: u64 = 4;
const RING_HDR_MASK: u64 = 8;
const RING_HDR_SIZE: u64 = 16;
const SQ_ENTRY_SIZE: u64 = 32;
const CQ_ENTRY_SIZE: u64 = 16;

/// The shim's ring is tiny: it submits one request and waits for it, so a depth
/// of 2 is ample (power of two, fits a frame easily).
const SHIM_RING_ENTRIES: u64 = 2;

/// SQ and CQ frames sit at the very top of the map window, above where the demos
/// map their working frames (which grow up from MAP_BASE), so the shim never
/// collides with the caller's mappings.
const SHIM_SQ_VA: u64 = MAP_END - 2 * PAGE_SIZE;
const SHIM_CQ_VA: u64 = MAP_END - PAGE_SIZE;

/// The process's lazily-set-up shim ring handle (None until the first read). A
/// user process is single-threaded, so this static is touched without races.
static mut SHIM_RING: u64 = SYS_ERR;

// Volatile accessors for the ring frames (shared with the kernel).
#[inline]
unsafe fn ring_r32(a: u64) -> u32 {
    core::ptr::read_volatile(a as *const u32)
}
#[inline]
unsafe fn ring_w32(a: u64, v: u32) {
    core::ptr::write_volatile(a as *mut u32, v)
}
#[inline]
unsafe fn ring_w64(a: u64, v: u64) {
    core::ptr::write_volatile(a as *mut u64, v)
}

/// Set up the shim's ring once: allocate + map an SQ and a CQ frame, register
/// them, and cache the handle. Returns the handle, or SYS_ERR if any step fails.
fn shim_ring() -> u64 {
    // SAFETY: single-threaded user process; the static is not shared.
    unsafe {
        if SHIM_RING != SYS_ERR {
            return SHIM_RING;
        }
        let sq_slot = sys_frame_alloc();
        if sq_slot == SYS_ERR || sys_frame_map(sq_slot, SHIM_SQ_VA) == SYS_ERR {
            return SYS_ERR;
        }
        let cq_slot = sys_frame_alloc();
        if cq_slot == SYS_ERR || sys_frame_map(cq_slot, SHIM_CQ_VA) == SYS_ERR {
            return SYS_ERR;
        }
        let handle = sys_ring_register(sq_slot, cq_slot, SHIM_RING_ENTRIES);
        if handle != SYS_ERR {
            SHIM_RING = handle;
        }
        handle
    }
}

/// Read `count` 512-byte sectors -- starting `sector_off` sectors into the
/// BlockRange capability at `range_slot` -- into the frame named by
/// `frame_slot`. BLOCKS until the device completes the read (other processes run
/// meanwhile). The device DMAs into the frame, so map it (sys_frame_map) to read
/// the bytes; the frame cap must carry the write right (frame_alloc grants it).
/// Sectors are named relative to the range, never absolutely. Returns BLK_OK, or
/// a BLK_E_* status.
///
/// This is a thin shim over the ring ABI: push one submission, ring the
/// doorbell, then wait and reap the single completion. Behaviourally identical
/// to a direct blocking read -- one request in flight, same blocking, same
/// status -- so every caller is unchanged.
#[inline]
pub fn sys_block_read(range_slot: u64, frame_slot: u64, sector_off: u64, count: u64) -> u64 {
    let handle = shim_ring();
    if handle == SYS_ERR {
        return BLK_E_DEV;
    }

    // SAFETY: SHIM_SQ_VA / SHIM_CQ_VA are this process's mapped ring frames; the
    // single-in-flight shim is the only writer of the SQ and reader of the CQ.
    unsafe {
        // Push one submission entry at the SQ tail.
        let mask = ring_r32(SHIM_SQ_VA + RING_HDR_MASK);
        let tail = ring_r32(SHIM_SQ_VA + RING_HDR_TAIL);
        let e = SHIM_SQ_VA + RING_HDR_SIZE + (tail & mask) as u64 * SQ_ENTRY_SIZE;
        ring_w32(e, 0); // op = RING_OP_READ
        ring_w32(e + 4, (count & 0xFFFF) as u32); // flags: count in the low 16 bits
        ring_w32(e + 8, range_slot as u32);
        ring_w32(e + 12, frame_slot as u32);
        ring_w64(e + 16, sector_off);
        ring_w64(e + 24, 0); // user_data: single in flight, cookie unused
        // Publish: bump the SQ tail so the kernel sees the new entry.
        ring_w32(SHIM_SQ_VA + RING_HDR_TAIL, tail.wrapping_add(1));
    }

    if sys_ring_submit(handle) == SYS_ERR {
        return BLK_E_DEV;
    }

    // Wait for and reap the single completion.
    loop {
        // SAFETY: as above.
        if let Some(status) = unsafe { shim_reap() } {
            return status;
        }
        if sys_ring_wait(handle) == SYS_ERR {
            return BLK_E_DEV;
        }
    }
}

/// Reap one completion from the shim CQ, if any: returns its status and advances
/// the CQ head (this process is the consumer). `None` when the CQ is empty.
///
/// SAFETY: the caller guarantees SHIM_CQ_VA is mapped.
unsafe fn shim_reap() -> Option<u64> {
    let head = ring_r32(SHIM_CQ_VA + RING_HDR_HEAD);
    let tail = ring_r32(SHIM_CQ_VA + RING_HDR_TAIL);
    if head == tail {
        return None;
    }
    let mask = ring_r32(SHIM_CQ_VA + RING_HDR_MASK);
    let e = SHIM_CQ_VA + RING_HDR_SIZE + (head & mask) as u64 * CQ_ENTRY_SIZE;
    let status = ring_r32(e + 8) as u64; // user_data at e+0 is unused (single in flight)
    // Advance the CQ head so the slot is reusable and ring_wait sees it drained.
    ring_w32(SHIM_CQ_VA + RING_HDR_HEAD, head.wrapping_add(1));
    Some(status)
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
// Input events: the single-subscription event-ring shim
// ---------------------------------------------------------------------------
// Input is the multishot event-ring path (Design/event_rings.md): a
// RING_OP_EVENT_SUB arms a standing subscription on an EventSource, and every
// event posts a completion into the ring's CQ. The old blocking `event_recv`
// int 0x80 op was retired in ABI v2.5; `sys_event_recv` is now a thin shim --
// subscribe once to the source, then ring_wait/reap one event per call --
// behaviourally identical to the old blocking read (one event at a time), so
// libinput and the input demos are unchanged. The async, many-event,
// multi-source path is the new capability layered on the same primitive (the
// block-read story, for input).

/// event_recv status. `EVENT_OK` means an event is in the returned word;
/// `EVENT_ERR` a bad slot, wrong capability kind, a missing read right, or a
/// ring-setup failure.
pub const EVENT_OK: u64 = 0;
pub const EVENT_ERR: u64 = 1;

/// Event kind (EVENT_KEY = 1, ...), the low byte of a packed event.
pub const EVENT_KEY: u8 = 1;
/// A mouse motion+button sample (Design/mouse_input.md S1): one packed event
/// per PS/2 packet, decode with `mouse_dx`/`mouse_dy`/`mouse_buttons`.
pub const EVENT_MOUSE_MOVE: u8 = 2;

// SQ op selectors for the event-ring control entries (event_rings.md s4).
const RING_OP_EVENT_SUB: u32 = 1;
const RING_OP_CANCEL: u32 = 2;

/// Drop-flag bit in an event completion's status (event_rings.md s5): set when
/// the kernel dropped events on a full CQ. The shim reaps one event at a time so
/// it never overflows, but it masks the flag off defensively before returning.
const EVT_DROPPED: u32 = 1 << 31;

/// The event ring is tiny -- one subscription, one event reaped at a time -- so
/// a depth of 4 is ample (power of two, fits a frame easily).
const EVT_RING_ENTRIES: u64 = 4;

/// The event ring's SQ/CQ frames sit just below the block shim's, at the top of
/// the map window, above where the demos map working frames (which grow up from
/// MAP_BASE), so neither shim collides with the caller's mappings.
const EVT_SQ_VA: u64 = MAP_END - 4 * PAGE_SIZE;
const EVT_CQ_VA: u64 = MAP_END - 3 * PAGE_SIZE;

/// The single subscription's cookie. Nonzero so it is never confused with an
/// all-zero subscribe-error status; the shim keys "is this an event?" off the
/// status kind byte, not the cookie.
const EVT_COOKIE: u64 = 1;

/// Lazily-set-up event ring handle, and the source slot the live subscription
/// names (SYS_ERR = none). A user process is single-threaded, so these statics
/// are touched without races.
static mut EVT_RING: u64 = SYS_ERR;
static mut EVT_SUB_SOURCE: u64 = SYS_ERR;

/// Set up the event ring once: allocate + map an SQ and a CQ frame, register
/// them, and cache the handle. Returns the handle, or SYS_ERR on any failure.
fn evt_ring() -> u64 {
    // SAFETY: single-threaded user process; the static is not shared.
    unsafe {
        if EVT_RING != SYS_ERR {
            return EVT_RING;
        }
        let sq_slot = sys_frame_alloc();
        if sq_slot == SYS_ERR || sys_frame_map(sq_slot, EVT_SQ_VA) == SYS_ERR {
            return SYS_ERR;
        }
        let cq_slot = sys_frame_alloc();
        if cq_slot == SYS_ERR || sys_frame_map(cq_slot, EVT_CQ_VA) == SYS_ERR {
            return SYS_ERR;
        }
        let handle = sys_ring_register(sq_slot, cq_slot, EVT_RING_ENTRIES);
        if handle != SYS_ERR {
            EVT_RING = handle;
        }
        handle
    }
}

/// Push one control entry (EVENT_SUB or CANCEL) onto the event ring's SQ and ring
/// the doorbell. For EVENT_SUB, `source_slot` names the EventSource cap; for
/// CANCEL it is unused. `cookie` is the subscription's user_data. Returns whether
/// the submit succeeded.
///
/// SAFETY: the caller guarantees EVT_SQ_VA is mapped (evt_ring succeeded).
unsafe fn evt_submit_op(handle: u64, op: u32, source_slot: u64, cookie: u64) -> bool {
    let mask = ring_r32(EVT_SQ_VA + RING_HDR_MASK);
    let tail = ring_r32(EVT_SQ_VA + RING_HDR_TAIL);
    let e = EVT_SQ_VA + RING_HDR_SIZE + (tail & mask) as u64 * SQ_ENTRY_SIZE;
    ring_w32(e, op);
    ring_w32(e + 4, 0); // flags
    ring_w32(e + 8, source_slot as u32); // range_slot field = EventSource cap (EVENT_SUB)
    ring_w32(e + 12, 0); // frame_slot
    ring_w64(e + 16, 0); // sector_off
    ring_w64(e + 24, cookie); // user_data = subscription cookie
    ring_w32(EVT_SQ_VA + RING_HDR_TAIL, tail.wrapping_add(1));
    sys_ring_submit(handle) != SYS_ERR
}

/// Reap one completion from the event CQ, if any: returns its raw 32-bit status
/// and advances the CQ head. `None` when the CQ is empty.
///
/// SAFETY: the caller guarantees EVT_CQ_VA is mapped.
unsafe fn evt_reap() -> Option<u32> {
    let head = ring_r32(EVT_CQ_VA + RING_HDR_HEAD);
    let tail = ring_r32(EVT_CQ_VA + RING_HDR_TAIL);
    if head == tail {
        return None;
    }
    let mask = ring_r32(EVT_CQ_VA + RING_HDR_MASK);
    let e = EVT_CQ_VA + RING_HDR_SIZE + (head & mask) as u64 * CQ_ENTRY_SIZE;
    let status = ring_r32(e + 8); // user_data at e+0 ignored (single subscription)
    ring_w32(EVT_CQ_VA + RING_HDR_HEAD, head.wrapping_add(1));
    Some(status)
}

/// Read the next input event from the EventSource capability at `source_slot`,
/// blocking until one arrives. Returns `(status, event)`: on `EVENT_OK` the
/// event is the packed word, unpacked with `event_kind` / `event_code` /
/// `event_value`. The kernel delivers raw scancodes; turning them into
/// characters is the caller's (library OS's) job.
///
/// Shim over the event-ring ABI: ensure a single subscription on `source_slot`
/// (lazily, re-subscribing if the caller switches sources), then ring_wait/reap
/// one event. A subscribe failure (e.g. a non-EventSource cap) surfaces as
/// `EVENT_ERR`: the kernel posts a zero-kind error completion the reap detects.
#[inline]
pub fn sys_event_recv(source_slot: u64) -> (u64, u64) {
    let handle = evt_ring();
    if handle == SYS_ERR {
        return (EVENT_ERR, 0);
    }
    // SAFETY: the statics are this single-threaded process's; the ring frames are
    // mapped (evt_ring succeeded), and the shim is their only writer/reader.
    unsafe {
        if EVT_SUB_SOURCE != source_slot {
            // Switching sources: drop the old subscription before arming the new.
            if EVT_SUB_SOURCE != SYS_ERR {
                evt_submit_op(handle, RING_OP_CANCEL, 0, EVT_COOKIE);
                EVT_SUB_SOURCE = SYS_ERR;
            }
            if !evt_submit_op(handle, RING_OP_EVENT_SUB, source_slot, EVT_COOKIE) {
                return (EVENT_ERR, 0);
            }
            EVT_SUB_SOURCE = source_slot;
        }
        loop {
            if let Some(status) = evt_reap() {
                // A real event always has a nonzero kind byte; a zero status is
                // the kernel's subscribe-error signal -> not actually subscribed.
                if status & 0xFF == 0 {
                    EVT_SUB_SOURCE = SYS_ERR;
                    return (EVENT_ERR, 0);
                }
                return (EVENT_OK, (status & !EVT_DROPPED) as u64);
            }
            if sys_ring_wait(handle) == SYS_ERR {
                return (EVENT_ERR, 0);
            }
        }
    }
}

/// Unpack a packed event's kind. For a key event this is `EVENT_KEY`.
#[inline]
pub fn event_kind(ev: u64) -> u8 {
    (ev & 0xFF) as u8
}

/// Unpack a packed event's device code. For a key event this is the raw Set-1
/// scancode byte.
#[inline]
pub fn event_code(ev: u64) -> u16 {
    ((ev >> 8) & 0xFFFF) as u16
}

/// Unpack a packed event's value. For a key event this is the make/break
/// convenience bit (1 = press, 0 = release); for a mouse event, the button
/// bitmask (see `mouse_buttons`, which is the same value under a clearer name).
#[inline]
pub fn event_value(ev: u64) -> u8 {
    ((ev >> 24) & 0xFF) as u8
}

/// Unpack a mouse event's X delta (one PS/2 packet's signed-byte motion, the
/// high byte of `event_code`). Only meaningful when `event_kind(ev) ==
/// EVENT_MOUSE_MOVE`.
#[inline]
pub fn mouse_dx(ev: u64) -> i8 {
    (event_code(ev) >> 8) as i8
}

/// Unpack a mouse event's Y delta (the low byte of `event_code`). Only
/// meaningful when `event_kind(ev) == EVENT_MOUSE_MOVE`.
#[inline]
pub fn mouse_dy(ev: u64) -> i8 {
    (event_code(ev) & 0xFF) as i8
}

/// Unpack a mouse event's button bitmask (bit 0/1/2 = left/right/middle).
/// Only meaningful when `event_kind(ev) == EVENT_MOUSE_MOVE`.
#[inline]
pub fn mouse_buttons(ev: u64) -> u8 {
    event_value(ev)
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
