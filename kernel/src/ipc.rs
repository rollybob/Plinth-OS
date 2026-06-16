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
//! A peer wakes it by writing the result into the saved frame (`rax` = status,
//! `rsi` = payload, `rdx` = transferred-cap slot; ABI v2) via
//! `scheduler::wake_with` and flipping it back to Ready. The non-blocking
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

use crate::capability::{CapObject, CapTable, Capability, RIGHT_RECV, RIGHT_SEND};
use crate::process;
use crate::scheduler::{self, TrapFrame, GP_RAX, GP_RDI, GP_RDX, GP_RSI};

/// Software-interrupt vector for blocking IPC. DPL 3 so ring 3 may `int`.
const IPC_VECTOR: usize = 0x80;

/// IPC operation selectors, passed in rax (mirroring the syscall-number ABI).
const IPC_SEND: u64 = 0;
const IPC_RECV: u64 = 1;
/// call = send a request and block for a reply (RPC); reply = answer the
/// caller named by a one-shot reply capability.
const IPC_CALL: u64 = 2;
const IPC_REPLY: u64 = 3;

/// IPC status, returned in rax -- separate from the message payload (rsi) and
/// the transferred-cap landing slot (rdx). Splitting status from the payload
/// (ABI v2) means no message value, not even `u64::MAX`, can be mistaken for an
/// error: a peer controls the full payload word, so an in-band error sentinel
/// would be ambiguous. The rendezvous path produces `IPC_OK`; `IPC_PEER_DIED`
/// is delivered by the death-time reaping (`reap_dying`) when a process blocked
/// on a peer outlives it. See Design/ipc.md.
const IPC_OK: u64 = 0;
const IPC_ERR: u64 = 1;
const IPC_PEER_DIED: u64 = 2;

/// "No capability" sentinel for the optional cap-transfer slot in `send`, and
/// for the landing-slot `recv` reports when no cap arrived. A real slot is a
/// small index, so `u64::MAX` is unambiguous. (libplinth mirrors it.)
const NO_CAP: u64 = u64::MAX;

/// Bounded endpoint table -- no heap, like the rest of Plinth.
const MAX_ENDPOINTS: usize = 8;

/// An intrusive FIFO of blocked waiters on one endpoint, plus the rendezvous
/// side it currently holds. The queue only ever holds waiters from ONE side at
/// a time (`are_senders` says which); the instant a peer arrives on the other
/// side they rendezvous, so it never mixes senders and receivers.
///
/// The links *between* waiters are deliberately NOT stored here: they live in
/// an external link array indexed by process slot (the scheduler's
/// `WAIT_LINKS`), passed into every method. That makes `WaitQueue` a pure
/// function of (its own fields, the link array) -- unit-testable with a plain
/// local array and no process table or hardware (see tests/ipc.rs), exactly as
/// `pick_next` is pure over a `[State; N]`.
#[derive(Clone, Copy)]
pub(crate) struct WaitQueue {
    head: Option<usize>,
    tail: Option<usize>,
    are_senders: bool,
}

impl WaitQueue {
    pub(crate) const fn empty() -> WaitQueue {
        WaitQueue { head: None, tail: None, are_senders: false }
    }

    /// Append `slot` to the FIFO and record which side is now waiting. The new
    /// tail's link is cleared; the previous tail (if any) is linked to it.
    pub(crate) fn enqueue(&mut self, slot: usize, are_senders: bool, links: &mut [Option<usize>]) {
        links[slot] = None;
        match self.tail {
            Some(tail) => links[tail] = Some(slot),
            None => self.head = Some(slot),
        }
        self.tail = Some(slot);
        self.are_senders = are_senders;
    }

    /// Remove and return the head, advancing to its linked successor. Returns
    /// None when the queue is empty.
    pub(crate) fn dequeue(&mut self, links: &[Option<usize>]) -> Option<usize> {
        let head = self.head?;
        self.head = links[head];
        if self.head.is_none() {
            self.tail = None;
        }
        Some(head)
    }

    /// Dequeue the head only if the queued side matches `want_senders`: a
    /// `recv` (wanting a sender) takes a waiting sender, a `send` (wanting a
    /// receiver) takes a waiting receiver, and neither touches a queue that
    /// holds its own side. This is the rendezvous-match decision.
    pub(crate) fn take_if(&mut self, want_senders: bool, links: &[Option<usize>]) -> Option<usize> {
        if self.head.is_some() && self.are_senders == want_senders {
            self.dequeue(links)
        } else {
            None
        }
    }

    /// True when no waiters are queued.
    pub(crate) fn is_empty(&self) -> bool {
        self.head.is_none()
    }
}

/// A rendezvous point: a capability-named endpoint, its wait queue, and a
/// reference count of the live capabilities that can reach it.
///
/// `senders`/`receivers` track how many live capabilities grant `RIGHT_SEND` /
/// `RIGHT_RECV` to this endpoint, summed across every process's table. They are
/// maintained symmetrically -- +1 on every mint of an `Endpoint` cap into a
/// table, -1 on every revoke/drain out of one (see `note_cap_added` /
/// `note_cap_removed`) -- so an IPC or spawn capability *transfer*, being a
/// revoke followed by a mint, nets to zero as the cap moves. The counts let the
/// kernel (a) free an endpoint slot once nothing can reach it (this stage) and
/// (b) wake peers stranded by a dead counterpart (Stage C2).
#[derive(Clone, Copy)]
pub(crate) struct Endpoint {
    in_use: bool,
    queue: WaitQueue,
    senders: u32,
    receivers: u32,
}

impl Endpoint {
    pub(crate) const fn empty() -> Endpoint {
        Endpoint { in_use: false, queue: WaitQueue::empty(), senders: 0, receivers: 0 }
    }

    /// Account a capability with `rights` becoming able to reach this endpoint.
    pub(crate) fn add_cap(&mut self, rights: u8) {
        if rights & RIGHT_SEND != 0 {
            self.senders += 1;
        }
        if rights & RIGHT_RECV != 0 {
            self.receivers += 1;
        }
    }

    /// Account a capability with `rights` no longer reaching this endpoint.
    /// A decrement below zero would mean an unmatched removal -- an accounting
    /// bug -- so it is caught loudly in debug and clamped in release.
    pub(crate) fn remove_cap(&mut self, rights: u8) {
        if rights & RIGHT_SEND != 0 {
            debug_assert!(self.senders > 0, "endpoint sender refcount underflow");
            self.senders = self.senders.saturating_sub(1);
        }
        if rights & RIGHT_RECV != 0 {
            debug_assert!(self.receivers > 0, "endpoint receiver refcount underflow");
            self.receivers = self.receivers.saturating_sub(1);
        }
    }

    /// True when nothing can reach this endpoint any more: no live caps and no
    /// queued waiters. (A queued waiter always still holds a cap, so the queue
    /// check is implied by the counts; it is kept explicit as defence in depth.)
    pub(crate) fn is_unreferenced(&self) -> bool {
        self.senders == 0 && self.receivers == 0 && self.queue.is_empty()
    }

    /// Given how many sender/receiver capabilities a dying process holds on this
    /// endpoint, would its death remove the *last* sender (or receiver)? If so,
    /// the opposite side's blocked waiters have lost their only possible
    /// counterpart and must be reaped. Pure decision used by `reap_dying`; the
    /// "wake-at-zero" half of the refcount machinery (free-at-zero is
    /// `is_unreferenced`).
    pub(crate) fn death_strands_peers(&self, dying_senders: u32, dying_receivers: u32) -> bool {
        let last_sender_gone =
            dying_senders > 0 && self.senders.saturating_sub(dying_senders) == 0;
        let last_receiver_gone =
            dying_receivers > 0 && self.receivers.saturating_sub(dying_receivers) == 0;
        last_sender_gone || last_receiver_gone
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
                *ep = Endpoint { in_use: true, ..Endpoint::empty() };
                return Some(i);
            }
        }
        None
    }
}

/// Account an `Endpoint` capability entering some process's table (a mint).
/// A no-op for every other capability kind, so call sites can pass any cap
/// without checking. Reached from every mint of an endpoint cap -- process
/// setup grants, the spawn handle, and the receiving half of a transfer.
pub(crate) fn note_cap_added(cap: &Capability) {
    if let CapObject::Endpoint { id } = cap.object {
        // SAFETY: single CPU, IF=0 at every accounting site.
        unsafe {
            (*addr_of_mut!(ENDPOINTS))[id].add_cap(cap.rights);
        }
    }
}

/// Account an `Endpoint` capability leaving a table (a revoke or drain). A
/// no-op for every other kind. `free_if_unreferenced` must be true ONLY when
/// the cap leaves *permanently* -- process teardown's drain -- where the last
/// reference disappearing means the slot can be reclaimed. A capability
/// *transfer* passes false: it is a revoke immediately followed by a matching
/// mint, so the count dips transiently and the endpoint must NOT be freed out
/// from under the in-flight move.
pub(crate) fn note_cap_removed(cap: &Capability, free_if_unreferenced: bool) {
    if let CapObject::Endpoint { id } = cap.object {
        // SAFETY: single CPU, IF=0 at every accounting site.
        unsafe {
            let eps = &mut *addr_of_mut!(ENDPOINTS);
            eps[id].remove_cap(cap.rights);
            if free_if_unreferenced && eps[id].is_unreferenced() {
                eps[id] = Endpoint::empty();
            }
        }
    }
}

/// Reclaim an endpoint slot if nothing references it. Used by `sys_spawn` to
/// release a freshly-created result endpoint when the child could not be
/// launched (no cap was ever minted for it, so it is unreferenced).
pub fn release_endpoint(id: usize) {
    // SAFETY: single CPU, IF=0.
    unsafe {
        let eps = &mut *addr_of_mut!(ENDPOINTS);
        if id < MAX_ENDPOINTS && eps[id].is_unreferenced() {
            eps[id] = Endpoint::empty();
        }
    }
}

/// Number of free (reclaimable) endpoint slots. Used by the boot path's
/// no-leak baseline checks, mirroring the frame-allocator's free count.
pub fn free_endpoint_count() -> usize {
    // SAFETY: scalar reads of the single-CPU table.
    unsafe { (*addr_of!(ENDPOINTS)).iter().filter(|ep| !ep.in_use).count() }
}

/// Death-time liveness reaping: `caps` belongs to a process that is exiting;
/// wake any *live* peer that was depending on it, so the peer observes
/// `IPC_PEER_DIED` instead of blocking forever. Runs in `on_exit` BEFORE
/// teardown drains the caps (hardening D5): it only wakes peers and never
/// touches refcounts -- teardown's drain still applies the decrements and frees
/// the slot. An endpoint with blocked waiters is never freed, because those
/// waiters still hold capabilities. Two strandings are possible:
///
///   (a) the dying process holds an unconsumed `Reply { caller }` (a server
///       that received a `call` but never replied): the caller is Blocked
///       awaiting a reply that will never come -- wake it.
///   (b) the dying process holds the last sender (or receiver) capability on an
///       endpoint whose queue holds blocked receivers (or senders): those peers
///       can never rendezvous -- wake them all.
pub(crate) fn reap_dying(caps: &CapTable) {
    // Tally this process's own per-endpoint references, and wake any caller it
    // still owed a reply.
    let mut dying_senders = [0u32; MAX_ENDPOINTS];
    let mut dying_receivers = [0u32; MAX_ENDPOINTS];
    for cap in caps.iter() {
        match cap.object {
            CapObject::Reply { caller } => {
                if scheduler::is_blocked(caller) {
                    scheduler::wake_with(caller, IPC_PEER_DIED, 0, NO_CAP);
                }
            }
            CapObject::Endpoint { id } if id < MAX_ENDPOINTS => {
                if cap.rights & RIGHT_SEND != 0 {
                    dying_senders[id] += 1;
                }
                if cap.rights & RIGHT_RECV != 0 {
                    dying_receivers[id] += 1;
                }
            }
            _ => {}
        }
    }
    // For each endpoint this process referenced, if its death removes the last
    // sender (or receiver), the opposite side's blocked waiters are stranded.
    // The queue only ever holds one side, and a queued waiter still holds a
    // capability (so it is counted) -- which is why a zero post-death count on
    // one side implies the queue cannot hold that same side.
    for id in 0..MAX_ENDPOINTS {
        // SAFETY: scalar read of the single-CPU table; copy out before waking.
        let ep = unsafe { (*addr_of!(ENDPOINTS))[id] };
        if ep.in_use && ep.death_strands_peers(dying_senders[id], dying_receivers[id]) {
            wake_all_stranded(id);
        }
    }
}

/// Drain endpoint `id`'s entire wait queue, waking each blocked waiter with
/// `IPC_PEER_DIED`. The caller has established that the queued side has lost its
/// only possible counterpart.
fn wake_all_stranded(id: usize) {
    loop {
        // SAFETY: single CPU, IF=0; ENDPOINTS and WAIT_LINKS are distinct
        // statics, so the borrows below do not alias.
        let woken = unsafe {
            let eps = &mut *addr_of_mut!(ENDPOINTS);
            scheduler::with_wait_links(|links| eps[id].queue.dequeue(links))
        };
        match woken {
            Some(slot) => scheduler::wake_with(slot, IPC_PEER_DIED, 0, NO_CAP),
            None => break,
        }
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
        IPC_CALL => ipc_call(a1, a2, frame as u64),
        IPC_REPLY => ipc_reply(a1, a2),
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
        // receiver, then wakes it with OK + the word + the landing slot.
        let landing = if cap_slot != NO_CAP {
            transfer_current_to_blocked(receiver, cap_slot)
        } else {
            NO_CAP
        };
        scheduler::wake_with(receiver, IPC_OK, msg, landing);
        return IPC_OK;
    }
    // No receiver waiting. If no live capability can ever receive here, blocking
    // would be forever -- report the dead peer instead (closes the race where
    // the only receiver died before this send was reached). Single CPU + IF=0
    // make this check-then-block atomic: no peer can die between them.
    if endpoint_receivers(id) == 0 {
        return IPC_PEER_DIED;
    }
    // Stash the word + cap slot and block as a sender.
    let cur = scheduler::current_slot();
    scheduler::set_pending(cur, msg, cap_slot, false);
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
        if scheduler::take_pending_call(sender) {
            // The sender is a caller awaiting a reply: mint it a one-shot
            // reply capability into us and leave it Blocked (only `reply`
            // wakes it). rsi carries the request, rdx the reply cap's slot.
            let reply_slot = mint_reply_cap_into_current(sender);
            write_rsi(frame_ptr, msg);
            write_rdx(frame_ptr, reply_slot);
            return IPC_OK;
        }
        // Plain send: pull any transferred cap, then wake the sender.
        let sender_cap = scheduler::take_pending_cap(sender);
        let landing = if sender_cap != NO_CAP {
            transfer_blocked_to_current(sender, sender_cap)
        } else {
            NO_CAP
        };
        scheduler::wake_with(sender, IPC_OK, 0, NO_CAP); // the sender's send() returns OK
        write_rsi(frame_ptr, msg); // the message word in our rsi
        write_rdx(frame_ptr, landing); // the landing slot in our rdx
        return IPC_OK;
    }
    // No sender waiting. If no live capability can ever send here, blocking
    // would be forever -- report the dead peer instead (closes the race where
    // the only sender died before this recv was reached, e.g. a spawned worker
    // that faulted before the parent reached its wait). Atomic under IF=0.
    if endpoint_senders(id) == 0 {
        return IPC_PEER_DIED;
    }
    // Block as a receiver. A later sender does the transfer and wakes us with
    // (msg, landing) via wake_with.
    let cur = scheduler::current_slot();
    enqueue_waiter(id, cur, false);
    scheduler::block_current(frame_ptr)
}

/// call(ep_slot, req): send a request and block for a reply (RPC). Delivers
/// `req` to a waiting server -- minting it a one-shot reply capability that
/// names this caller -- and blocks until the server replies; or, if no server
/// is waiting, blocks as a call-sender until one receives the request. Returns
/// the reply word (delivered into rax by `reply` via `wake_with`).
fn ipc_call(ep_slot: u64, req: u64, frame_ptr: u64) -> u64 {
    let Some(id) = endpoint_id_for(ep_slot, RIGHT_SEND) else {
        return IPC_ERR;
    };
    if let Some(server) = take_waiting_receiver(id) {
        // A server is waiting: hand over the request with a reply cap naming
        // us, wake the server, and block awaiting the reply.
        let caller = scheduler::current_slot();
        let reply_slot = mint_reply_cap_into_blocked(server, caller);
        scheduler::wake_with(server, IPC_OK, req, reply_slot);
    } else {
        // No server is waiting. If no live capability can ever receive (serve)
        // here, the call can never be answered -- report the dead peer now
        // rather than block forever. Atomic under IF=0.
        if endpoint_receivers(id) == 0 {
            return IPC_PEER_DIED;
        }
        // Block as a call-sender. The receiver will mint the reply cap and
        // leave us blocked until `reply` wakes us.
        let cur = scheduler::current_slot();
        scheduler::set_pending(cur, req, NO_CAP, true);
        enqueue_waiter(id, cur, true);
    }
    // Either way the caller blocks now -- it is not enqueued as a receiver, so
    // only its reply capability can wake it.
    scheduler::block_current(frame_ptr)
}

/// reply(reply_slot, msg): wake the caller named by the one-shot reply
/// capability at `reply_slot`, delivering `msg` as its `call` result, and
/// consume the capability. No endpoint right is needed -- holding the reply
/// cap is the authority. Returns 0, or IPC_ERR if the slot is not a live reply
/// cap or its caller is no longer awaiting.
fn ipc_reply(reply_slot: u64, msg: u64) -> u64 {
    let caller = {
        let guard = process::CURRENT.lock();
        match guard
            .as_ref()
            .and_then(|p| p.caps.lookup(reply_slot as usize, 0).ok())
        {
            Some(cap) => match cap.object {
                CapObject::Reply { caller } => caller,
                _ => return IPC_ERR,
            },
            None => return IPC_ERR,
        }
    };
    // The caller is pinned Blocked until replied; if it is not Blocked the cap
    // is stale (should not happen given one-shot consumption).
    if !scheduler::is_blocked(caller) {
        return IPC_ERR;
    }
    // The caller's `call` resumes with OK + the reply word in rsi.
    scheduler::wake_with(caller, IPC_OK, msg, NO_CAP);
    // One-shot: consume the reply capability.
    let mut guard = process::CURRENT.lock();
    if let Some(p) = guard.as_mut() {
        let _ = p.caps.revoke(reply_slot as usize);
    }
    IPC_OK
}

/// Mint a one-shot reply capability naming `caller` into the current (server)
/// process. Returns its slot, or NO_CAP if the table is full.
fn mint_reply_cap_into_current(caller: usize) -> u64 {
    let cap = Capability { object: CapObject::Reply { caller }, rights: 0 };
    let landing = {
        let mut guard = process::CURRENT.lock();
        guard
            .as_mut()
            .and_then(|p| p.caps.mint(cap.object, cap.rights).ok())
    };
    landing.map(|l| l as u64).unwrap_or(NO_CAP)
}

/// Mint a one-shot reply capability naming `caller` into the blocked server at
/// `server`. Returns its slot, or NO_CAP if that table is full.
fn mint_reply_cap_into_blocked(server: usize, caller: usize) -> u64 {
    let cap = Capability { object: CapObject::Reply { caller }, rights: 0 };
    scheduler::mint_into_blocked(server, cap)
        .map(|l| l as u64)
        .unwrap_or(NO_CAP)
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
    // The revoke above is the give half of the move; account it (no free --
    // the matching mint below re-references the endpoint).
    note_cap_removed(&cap, false);
    match scheduler::mint_into_blocked(receiver, cap) {
        Some(landing) => {
            note_cap_added(&cap);
            landing as u64
        }
        None => {
            // Receiver's table is full: hand the capability back to the sender.
            let mut guard = process::CURRENT.lock();
            if let Some(p) = guard.as_mut() {
                let _ = p.caps.mint(cap.object, cap.rights);
            }
            note_cap_added(&cap);
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
    // Give half of the move; account it (no free -- the mint below re-refs).
    note_cap_removed(&cap, false);
    let landing = {
        let mut guard = process::CURRENT.lock();
        guard
            .as_mut()
            .and_then(|p| p.caps.mint(cap.object, cap.rights).ok())
    };
    match landing {
        Some(l) => {
            note_cap_added(&cap);
            l as u64
        }
        None => {
            let _ = scheduler::mint_into_blocked(sender, cap);
            note_cap_added(&cap);
            NO_CAP
        }
    }
}

/// Write `val` into the rsi slot of the trap frame at `frame_ptr`, so a
/// non-blocking `recv`/`call` returns the message payload there (the stub
/// restores rsi on iretq). rax carries the status; rsi the payload (ABI v2).
fn write_rsi(frame_ptr: u64, val: u64) {
    // SAFETY: frame_ptr is this call's trap frame on the current process's
    // kernel stack; valid for this call.
    unsafe {
        (*(frame_ptr as *mut TrapFrame)).gp[GP_RSI] = val;
    }
}

/// Write `val` into the rdx slot of the trap frame at `frame_ptr`, so the
/// non-blocking `recv` returns the transferred-cap landing slot in rdx (the
/// stub restores rdx on iretq).
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

/// Count of live capabilities that can send on / receive from endpoint `id`.
/// The block-time liveness check reads these: a process about to wait for a
/// counterpart that can never arrive (zero on the opposite side) is told the
/// peer is gone instead of blocking forever.
fn endpoint_senders(id: usize) -> u32 {
    // SAFETY: scalar read of the single-CPU table.
    unsafe { (*addr_of!(ENDPOINTS))[id].senders }
}

fn endpoint_receivers(id: usize) -> u32 {
    // SAFETY: scalar read of the single-CPU table.
    unsafe { (*addr_of!(ENDPOINTS))[id].receivers }
}

/// Dequeue a waiting receiver if the queue holds receivers; else None.
fn take_waiting_receiver(id: usize) -> Option<usize> {
    take_waiting(id, false)
}

/// Dequeue a waiting sender if the queue holds senders; else None.
fn take_waiting_sender(id: usize) -> Option<usize> {
    take_waiting(id, true)
}

/// Take endpoint `id`'s head waiter iff it is on the `want_senders` side,
/// driving the pure `WaitQueue` over the scheduler's link array.
fn take_waiting(id: usize, want_senders: bool) -> Option<usize> {
    // SAFETY: single CPU, IF=0. ENDPOINTS and WAIT_LINKS are distinct statics,
    // so the two mutable borrows below do not alias.
    unsafe {
        let eps = &mut *addr_of_mut!(ENDPOINTS);
        scheduler::with_wait_links(|links| eps[id].queue.take_if(want_senders, links))
    }
}

/// Append `slot` to endpoint `id`'s FIFO wait queue, recording which side is
/// waiting. The links live in the scheduler's `WAIT_LINKS` array.
fn enqueue_waiter(id: usize, slot: usize, are_senders: bool) {
    // SAFETY: single CPU, IF=0. ENDPOINTS and WAIT_LINKS are distinct statics,
    // so the two mutable borrows below do not alias.
    unsafe {
        let eps = &mut *addr_of_mut!(ENDPOINTS);
        scheduler::with_wait_links(|links| eps[id].queue.enqueue(slot, are_senders, links));
    }
}
