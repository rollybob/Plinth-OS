//! Synchronous IPC: capability-named endpoints (Phase 2, step 2, Stage 1).
//!
//! An endpoint is a bufferless rendezvous point. `send` and `recv` meet on it:
//! whichever arrives second completes the exchange immediately; whichever
//! arrives first blocks until its peer shows up. The kernel never stores a
//! message -- only a *blocked* thread waits, and a blocked sender holds its
//! own message in its process slot. Bulk data is meant to ride shared frames
//! (a later stage); the message here is a single machine word.
//!
//! ## Why these enter through `int 0x80`, not `syscall`
//!
//! A blocking operation must be able to suspend mid-call and be resumed later
//! with a return value. The scheduler resumes a process by restoring a full
//! register *trap frame* and `iretq`-ing -- exactly what an interrupt entry
//! saves, but NOT what the `syscall`/`sysret` fast path saves (it preserves
//! only what `sysret` needs). So the blocking IPC ops enter via a software
//! interrupt gate (vector 0x80, DPL 3): the stub captures a full trap frame,
//! and a blocked process slots straight into the scheduler's existing
//! Blocked/`sched_resume` machinery with no change to the context-switch core.
//! A peer wakes it by writing the result into the saved frame's rax slot
//! (`scheduler::wake_with`) and flipping it back to Ready. The non-blocking
//! syscalls keep using the fast `syscall` path.
//!
//! Endpoint creation is the kernel's job in this stage: the boot path makes an
//! endpoint and grants a capability to it into each demo process. A
//! process-facing `endpoint_create` syscall arrives when processes need to
//! make their own (with `spawn`).

use core::arch::global_asm;
use core::ptr::{addr_of, addr_of_mut};

use x86_64::structures::idt::InterruptDescriptorTable;
use x86_64::{PrivilegeLevel, VirtAddr};

use crate::capability::{CapObject, RIGHT_RECV, RIGHT_SEND};
use crate::process;
use crate::scheduler::{self, TrapFrame, GP_RAX, GP_RDI, GP_RDX, GP_RSI};

/// Software-interrupt vector for blocking IPC. DPL 3 so ring 3 may `int`.
const IPC_VECTOR: usize = 0x80;

/// IPC operation selectors, passed in rax (mirroring the syscall-number ABI).
const IPC_SEND: u64 = 0;
const IPC_RECV: u64 = 1;

/// Error sentinel, same convention as the syscall layer (`u64::MAX`).
const IPC_ERR: u64 = u64::MAX;

/// "No capability" sentinel for the optional cap-transfer slot in `send`, and
/// for the landing-slot `recv` reports when no cap arrived. A real slot is a
/// small index, so `u64::MAX` is unambiguous. (libplinth mirrors it.)
const NO_CAP: u64 = u64::MAX;

/// Bounded endpoint table -- no heap, like the rest of Plinth.
const MAX_ENDPOINTS: usize = 8;

/// A rendezvous point. It holds at most a queue of blocked threads on ONE
/// side at a time (`are_senders` says which); the moment a peer arrives on the
/// other side they rendezvous and the queue never holds both. The queue is
/// intrusive -- the links live in the process slots (scheduler.rs) -- so an
/// endpoint stores only head/tail slot indices.
#[derive(Clone, Copy)]
struct Endpoint {
    in_use: bool,
    head: Option<usize>,
    tail: Option<usize>,
    are_senders: bool,
}

impl Endpoint {
    const fn empty() -> Endpoint {
        Endpoint { in_use: false, head: None, tail: None, are_senders: false }
    }
}

/// The endpoint table. Single CPU + IF=0 in all IPC code make the bare
/// `static mut` safe (the same discipline as the scheduler's table).
static mut ENDPOINTS: [Endpoint; MAX_ENDPOINTS] = [const { Endpoint::empty() }; MAX_ENDPOINTS];

global_asm!(
    r#"
.global ipc_entry
ipc_entry:
    // int 0x80 from ring 3 via an interrupt gate: IF=0 on entry, no error
    // code (the CPU pushed ss,rsp,rflags,cs,rip). Save the 15 GP regs below
    // so rsp points at a full TrapFrame -- identical layout to timer_entry,
    // so the scheduler can resume a blocked process the same way.
    push r15
    push r14
    push r13
    push r12
    push r11
    push r10
    push r9
    push r8
    push rbp
    push rdi
    push rsi
    push rdx
    push rcx
    push rbx
    push rax
    mov rdi, rsp        // &TrapFrame
    cld
    // 5 CPU-pushed words + 15 here = 16-aligned, as the call requires (no
    // error code, so no sub rsp,8 -- same as timer_entry).
    call ipc_dispatch   // rax = result; never returns if the op blocked
    mov [rsp], rax      // overwrite the saved rax with the result
    pop rax
    pop rbx
    pop rcx
    pop rdx
    pop rsi
    pop rdi
    pop rbp
    pop r8
    pop r9
    pop r10
    pop r11
    pop r12
    pop r13
    pop r14
    pop r15
    iretq
"#
);

extern "C" {
    fn ipc_entry();
}

/// Install the IPC interrupt gate. DPL 3 so a ring-3 `int 0x80` is allowed;
/// it is still an interrupt gate (IF cleared on entry), so IPC dispatch runs
/// non-preemptibly like every other kernel entry.
pub fn register(idt: &mut InterruptDescriptorTable) {
    // SAFETY: ipc_entry is the naked stub above; it hand-manages the
    // CPU-pushed frame and returns via iretq (or never returns, on block).
    unsafe {
        idt[IPC_VECTOR]
            .set_handler_addr(VirtAddr::new(ipc_entry as *const () as u64))
            .set_privilege_level(PrivilegeLevel::Ring3);
    }
}

/// Allocate an endpoint, returning its id. Used by the boot path to set up the
/// demo; bounded, no heap.
pub fn create_endpoint() -> Option<usize> {
    // SAFETY: single CPU, IF=0 at setup time.
    unsafe {
        let eps = &mut *addr_of_mut!(ENDPOINTS);
        for (i, ep) in eps.iter_mut().enumerate() {
            if !ep.in_use {
                *ep = Endpoint { in_use: true, head: None, tail: None, are_senders: false };
                return Some(i);
            }
        }
        None
    }
}

/// The IPC interrupt dispatcher. Reached only from `ipc_entry`. Reads the
/// operation and its args from the saved trap frame (rax = op, rdi/rsi =
/// args), and either returns a result (non-blocking) or never returns (the
/// op blocked and switched to another process).
#[no_mangle]
extern "C" fn ipc_dispatch(frame: *mut TrapFrame) -> u64 {
    // SAFETY: the stub passes a pointer to the trap frame it built on the
    // current process's kernel stack; valid for this call. rax = op, rdi/rsi =
    // args, rdx = the optional cap slot to transfer (send only).
    let (op, a1, a2, a3) = unsafe {
        let f = &*frame;
        (f.gp[GP_RAX], f.gp[GP_RDI], f.gp[GP_RSI], f.gp[GP_RDX])
    };
    match op {
        IPC_SEND => ipc_send(a1, a2, a3, frame as u64),
        IPC_RECV => ipc_recv(a1, frame as u64),
        _ => IPC_ERR,
    }
}

/// send(ep_slot, msg, cap_slot): deliver `msg` (and, if `cap_slot != NO_CAP`,
/// transfer that capability) to a waiting receiver and return 0, or block
/// until one appears. `frame_ptr` is this call's saved trap frame, used to
/// resume the sender once a receiver completes the rendezvous.
fn ipc_send(ep_slot: u64, msg: u64, cap_slot: u64, frame_ptr: u64) -> u64 {
    let Some(id) = endpoint_id_for(ep_slot, RIGHT_SEND) else {
        return IPC_ERR;
    };
    if let Some(receiver) = take_waiting_receiver(id) {
        // Rendezvous now: this sender does the transfer into the blocked
        // receiver, then wakes it with the word and the landing slot.
        let landing = if cap_slot != NO_CAP {
            transfer_current_to_blocked(receiver, cap_slot)
        } else {
            NO_CAP
        };
        scheduler::wake_with(receiver, msg, landing);
        return 0;
    }
    // No receiver waiting: stash the word + cap slot and block as a sender.
    let cur = scheduler::current_slot();
    scheduler::set_pending(cur, msg, cap_slot);
    enqueue_waiter(id, cur, true);
    scheduler::block_current(frame_ptr)
}

/// recv(ep_slot): return a waiting sender's message (rax) and, if it sent a
/// capability, that cap's landing slot in this process's table (rdx; NO_CAP
/// otherwise). Blocks until a sender appears.
fn ipc_recv(ep_slot: u64, frame_ptr: u64) -> u64 {
    let Some(id) = endpoint_id_for(ep_slot, RIGHT_RECV) else {
        return IPC_ERR;
    };
    if let Some(sender) = take_waiting_sender(id) {
        let msg = scheduler::take_pending(sender);
        let sender_cap = scheduler::take_pending_cap(sender);
        // Rendezvous now: this receiver pulls the transfer out of the blocked
        // sender into its own table.
        let landing = if sender_cap != NO_CAP {
            transfer_blocked_to_current(sender, sender_cap)
        } else {
            NO_CAP
        };
        scheduler::wake_with(sender, 0, NO_CAP); // the sender's send() returns 0
        write_rdx(frame_ptr, landing); // report the landing slot in our rdx
        return msg;
    }
    // No sender waiting: block as a receiver. A later sender does the transfer
    // and wakes us with (msg, landing) via wake_with.
    let cur = scheduler::current_slot();
    enqueue_waiter(id, cur, false);
    scheduler::block_current(frame_ptr)
}

/// Move the capability at `cap_slot` from the current (sending) process into
/// the blocked receiver at `receiver`. Returns the landing slot, or NO_CAP on
/// failure (best-effort restore to the sender). Takes/releases the CURRENT
/// lock around the sender side so none is held across the mint.
fn transfer_current_to_blocked(receiver: usize, cap_slot: u64) -> u64 {
    let cap = {
        let mut guard = process::CURRENT.lock();
        match guard.as_mut() {
            Some(p) => process::revoke_and_unmap(p, cap_slot as usize),
            None => None,
        }
    };
    let Some(cap) = cap else {
        return NO_CAP;
    };
    match scheduler::mint_into_blocked(receiver, cap) {
        Some(landing) => landing as u64,
        None => {
            // Receiver's table is full: hand the capability back to the sender.
            let mut guard = process::CURRENT.lock();
            if let Some(p) = guard.as_mut() {
                let _ = p.caps.mint(cap.object, cap.rights);
            }
            NO_CAP
        }
    }
}

/// Move the capability at `cap_slot` from the blocked sender at `sender` into
/// the current (receiving) process. Returns the landing slot, or NO_CAP on
/// failure (best-effort restore to the still-blocked sender).
fn transfer_blocked_to_current(sender: usize, cap_slot: u64) -> u64 {
    let Some(cap) = scheduler::revoke_from_blocked(sender, cap_slot as usize) else {
        return NO_CAP;
    };
    let landing = {
        let mut guard = process::CURRENT.lock();
        guard
            .as_mut()
            .and_then(|p| p.caps.mint(cap.object, cap.rights).ok())
    };
    match landing {
        Some(l) => l as u64,
        None => {
            let _ = scheduler::mint_into_blocked(sender, cap);
            NO_CAP
        }
    }
}

/// Write `val` into the rdx slot of the trap frame at `frame_ptr`, so the
/// non-blocking `recv` returns it in rdx (the stub restores rdx on iretq).
fn write_rdx(frame_ptr: u64, val: u64) {
    // SAFETY: frame_ptr is this call's trap frame on the current process's
    // kernel stack; valid for this call.
    unsafe {
        (*(frame_ptr as *mut TrapFrame)).gp[GP_RDX] = val;
    }
}

/// Resolve `slot` in the current process's table to a live endpoint id,
/// requiring `right` (RIGHT_SEND or RIGHT_RECV). Takes and releases the
/// CURRENT lock entirely here so no lock is held across a later block.
fn endpoint_id_for(slot: u64, right: u8) -> Option<usize> {
    let guard = process::CURRENT.lock();
    let cap = guard.as_ref()?.caps.lookup(slot as usize, right).ok()?;
    match cap.object {
        CapObject::Endpoint { id } if id < MAX_ENDPOINTS && endpoint_in_use(id) => Some(id),
        _ => None,
    }
}

fn endpoint_in_use(id: usize) -> bool {
    // SAFETY: scalar read of the single-CPU table.
    unsafe { (*addr_of!(ENDPOINTS))[id].in_use }
}

/// Dequeue a waiting receiver if the queue holds receivers; else None.
fn take_waiting_receiver(id: usize) -> Option<usize> {
    // SAFETY: single CPU, IF=0.
    unsafe {
        let eps = &*addr_of!(ENDPOINTS);
        if eps[id].head.is_some() && !eps[id].are_senders {
            Some(dequeue_waiter(id))
        } else {
            None
        }
    }
}

/// Dequeue a waiting sender if the queue holds senders; else None.
fn take_waiting_sender(id: usize) -> Option<usize> {
    // SAFETY: single CPU, IF=0.
    unsafe {
        let eps = &*addr_of!(ENDPOINTS);
        if eps[id].head.is_some() && eps[id].are_senders {
            Some(dequeue_waiter(id))
        } else {
            None
        }
    }
}

/// Append `slot` to endpoint `id`'s FIFO wait queue, recording which side is
/// waiting. The link lives in the slot (scheduler.rs).
fn enqueue_waiter(id: usize, slot: usize, are_senders: bool) {
    scheduler::set_wait_next(slot, None);
    // SAFETY: single CPU, IF=0.
    unsafe {
        let eps = &mut *addr_of_mut!(ENDPOINTS);
        match eps[id].tail {
            Some(tail) => scheduler::set_wait_next(tail, Some(slot)),
            None => eps[id].head = Some(slot),
        }
        eps[id].tail = Some(slot);
        eps[id].are_senders = are_senders;
    }
}

/// Remove and return the head of endpoint `id`'s wait queue. Caller has
/// checked the queue is non-empty.
fn dequeue_waiter(id: usize) -> usize {
    // SAFETY: single CPU, IF=0; caller guarantees a non-empty queue.
    unsafe {
        let eps = &mut *addr_of_mut!(ENDPOINTS);
        let head = eps[id].head.expect("dequeue from empty endpoint queue");
        eps[id].head = scheduler::wait_next(head);
        if eps[id].head.is_none() {
            eps[id].tail = None;
        }
        head
    }
}
