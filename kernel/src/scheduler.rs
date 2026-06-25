//! Preemptive round-robin scheduler (Phase 2, Stage 2).
//!
//! This is where Plinth stops running one process at a time. A periodic timer
//! interrupt (timer.rs) preempts whatever is in ring 3, the kernel saves the
//! full interrupted context, picks another runnable process, switches address
//! space and kernel stack, and resumes it. That is the defining capability of
//! a multiplexing kernel.
//!
//! ## Model
//!
//! - `process::current()` keeps its meaning -- the process on the CPU right now
//!   -- so the syscall and fault surfaces are unchanged. The processes that
//!   are *not* running live in `TABLE` (Ready), with the running one's slot
//!   holding `None` (its `Process` is in `CURRENT`). A switch moves the
//!   running `Process` out of `CURRENT` into its slot and the next one in.
//! - Each scheduled process has its OWN kernel stack (KSTACKS); on a switch
//!   `TSS.rsp0` is repointed at it (gdt::set_kernel_stack) so the next ring-3
//!   interrupt frame lands on the right stack. A shared kernel stack would
//!   clobber a suspended process's saved frame.
//! - Non-preemptible kernel (design D2): the timer only reschedules when it
//!   interrupts ring 3. Kernel code always runs with IF=0 (SFMask on
//!   syscalls, interrupt gate on the timer), so it is never reentered and the
//!   single shared syscall stack is always empty at switch time.
//!
//! ## Context-switch asm
//!
//! Closely modeled on `fault.rs`'s naked-stub pattern. The trap frame is the
//! same GP layout as `fault.rs::RawTrap` but WITHOUT an error code (IRQ0 does
//! not push one):
//!
//!   [rax,rbx,rcx,rdx,rsi,rdi,rbp,r8..r15] [rip,cs,rflags,rsp,ss]
//!
//! `timer_entry` captures it, `timer_tick` decides, and a uniform tail
//! restores whichever frame `timer_tick` returns (the same process, or the
//! next one). `sched_resume` is that restore as a standalone one-way jump,
//! reused for exit-driven switches and first run. `sched_start` /
//! `sched_return_to_kernel` are the kernel-side setjmp/longjmp that bracket
//! the whole demo (mirroring usermode.rs's enter_user / kernel_resume).

use core::arch::global_asm;
use core::fmt::Write;
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use x86_64::structures::idt::InterruptDescriptorTable;
use x86_64::VirtAddr;

use crate::bkl;
use crate::capability::Capability;
use crate::gdt;
use crate::ipc;
use crate::memory;
use crate::percpu;
use crate::process::{self, Process, MAX_BOOT_FRAMES, USER_STACK_TOP};
use crate::serial;
use crate::timer;

/// Maximum number of processes the scheduler can hold at once. Bounded so the
/// table and the kernel stacks live in fixed arrays (no heap, like the rest
/// of Plinth).
pub const MAX_PROCESSES: usize = 4;

/// Quantum, in timer ticks. 1 = switch on every ring-3 tick (maximal
/// interleaving). The kernel is correct for any value; this only affects how
/// finely time is sliced.
const QUANTUM: u64 = 1;

/// Per-process kernel stack size. Generous; these take interrupts and the
/// fault-handling path, same as the synchronous RSP0 stack.
const KSTACK_SIZE: usize = 16 * 4096;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// No process here.
    Empty,
    /// Runnable but not on the CPU; its `Process` and saved context live in
    /// the slot.
    Ready,
    /// On the CPU right now; its `Process` is in `process::current()` and its
    /// saved context does not exist yet (it is live in registers).
    Running,
    /// Waiting on an IPC endpoint; out of the round-robin rotation until a
    /// peer wakes it. Like Ready, its `Process` and saved trap frame live in
    /// the slot; unlike Ready, `pick_next` skips it. Woken by `wake_with`,
    /// which writes the result into the saved frame and flips it back to
    /// Ready (see ipc.rs).
    Blocked,
}

/// One process's scheduler-side bookkeeping. The `Process` itself is here only
/// while the process is suspended (Ready); while it Runs it lives in CURRENT.
struct Slot {
    state: State,
    process: Option<Process>,
    /// Kernel rsp where this process's saved trap frame sits (meaningful
    /// while Ready or Blocked).
    kernel_rsp: u64,
    /// (va, phys) pairs the kernel mapped for this process, for teardown.
    boot_frames: [Option<(u64, u64)>; MAX_BOOT_FRAMES],
    /// Message a blocked *sender* is waiting to hand over (ipc.rs).
    pending_msg: u64,
    /// Capability slot a blocked *sender* wants transferred with the message,
    /// or `NO_CAP` for a word-only send (ipc.rs owns the sentinel).
    pending_cap: u64,
    /// True if a blocked sender is a `call` (awaits a reply) rather than a
    /// plain `send` (which a receiver wakes). Set when blocking, read by the
    /// receiver to decide whether to wake the sender or mint it a reply cap.
    pending_call: bool,
}

impl Slot {
    const fn empty() -> Slot {
        Slot {
            state: State::Empty,
            process: None,
            kernel_rsp: 0,
            boot_frames: [None; MAX_BOOT_FRAMES],
            pending_msg: 0,
            pending_cap: u64::MAX,
            pending_call: false,
        }
    }
}

// The field is storage only -- referenced by address, never read as data.
#[repr(align(16))]
struct KStack(#[allow(dead_code)] [u8; KSTACK_SIZE]);

/// Suspended/empty processes. The running process's slot holds `state =
/// Running` and `process = None`. Single CPU + IF=0 in all kernel code make
/// the bare `static mut` safe (same discipline as usermode.rs / fault.rs).
static mut TABLE: [Slot; MAX_PROCESSES] = [const { Slot::empty() }; MAX_PROCESSES];

/// Intrusive next-links for the IPC wait queues (ipc.rs). Indexed by process
/// slot: `WAIT_LINKS[s]` is the slot after `s` in whatever endpoint queue `s`
/// is enqueued on. Meaningful only while a slot is Blocked and enqueued; the
/// link nodes are the process slots themselves, so the wait queue needs no
/// heap. Kept as a standalone array (rather than a `Slot` field) so the pure
/// `WaitQueue` structure can be handed the whole link store as one slice. Same
/// single-CPU + IF=0 discipline as `TABLE`.
static mut WAIT_LINKS: [Option<usize>; MAX_PROCESSES] = [None; MAX_PROCESSES];

/// Index in TABLE of the process currently on each core (Stage B2.3, D6) --
/// `CURRENT_SLOT[percpu::core_id()]` is the slot for THIS core. Plain Rust
/// reads/writes only (no naked-asm stub touches this), so an ordinary
/// per-core array suffices -- no need to live in `percpu::PerCpu`.
static mut CURRENT_SLOT: [usize; percpu::MAX_CORES] = [0; percpu::MAX_CORES];

/// Each core's own ready/running queue (`Design/smp_scaling.md` S1, replacing
/// D5's claim-on-first-run `owner` tag): `CORE_QUEUE[core][i]` is `Some(table
/// slot)` if position `i` in `core`'s queue holds that process, `None` if the
/// position is unused. A `TABLE` slot index appears in exactly one core's
/// queue, from the moment it is set up (`setup_process` is handed its home
/// core) until it exits (`on_exit` removes it). Sized `MAX_PROCESSES` per
/// core, not `MAX_PROCESSES / MAX_CORES`, because nothing stops every process
/// from ending up homed on the same core -- the worst case is still bounded by
/// the total process count. `pick_next` operates on *positions* within one
/// core's queue, never on raw `TABLE` indices, which is the actual difference
/// from the tag scheme this replaces (see `core_states`/`core_table_slot`).
static mut CORE_QUEUE: [[Option<usize>; MAX_PROCESSES]; percpu::MAX_CORES] =
    [[None; MAX_PROCESSES]; percpu::MAX_CORES];

/// Round-robin cursor for home-core assignment (`next_home_core`). Starts at 1
/// (not 0) so a fresh boot's first non-slot-0 process does not land back on
/// the BSP, which `run` already homes slot 0 to explicitly; persists across
/// `run` calls so successive demos keep spreading load rather than always
/// restarting from the same core.
static mut NEXT_HOME_CORE: u32 = 1;

/// Ticks accumulated toward the current quantum, per core.
static mut TICKS_IN_QUANTUM: [u64; percpu::MAX_CORES] = [0; percpu::MAX_CORES];

/// True while `run` is driving processes. The death path (process::exit_current)
/// reads it to choose between scheduling the next process and the synchronous
/// kernel_resume longjmp.
static SCHEDULER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// One kernel stack per process slot.
static mut KSTACKS: [KStack; MAX_PROCESSES] = [const { KStack([0; KSTACK_SIZE]) }; MAX_PROCESSES];

/// Top (highest address) of slot `i`'s kernel stack.
fn kstack_top(slot: usize) -> u64 {
    let base = addr_of!(KSTACKS) as u64;
    base + ((slot + 1) * KSTACK_SIZE) as u64
}

/// The saved/fabricated context the asm restores: 15 GP registers followed by
/// an iretq frame. Field order matches the stub's push order and the pop
/// order in `sched_resume` / the `timer_entry` tail.
#[repr(C)]
pub struct TrapFrame {
    pub gp: [u64; 15],
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

/// Indices within `gp`, in the stub's push order
/// (rax,rbx,rcx,rdx,rsi,rdi,rbp,r8..r15). `GP_RDI` is where the scheduler
/// plants the process id so `_start` receives it as arg0; `GP_RAX`/`GP_RSI`
/// let the IPC layer read syscall-style args from a trap frame and write the
/// result back (ipc.rs).
pub const GP_RAX: usize = 0;
pub const GP_RDX: usize = 3;
pub const GP_RSI: usize = 4;
pub const GP_RDI: usize = 5;

global_asm!(
    r#"
.global timer_entry
timer_entry:
    // IRQ0 via interrupt gate: IF=0 on entry, no error code. CPU pushed
    // ss,rsp,rflags,cs,rip. Push the 15 GP regs below so rsp points at a full
    // TrapFrame (gp[0]=rax lowest). Mirrors fault.rs page_fault_entry minus
    // the error code.
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
    cld                 // Rust expects DF clear
    // Alignment: rsp0 is 16-aligned; the CPU pushed 5 words (no error code,
    // unlike #PF) and we pushed 15, so rsp is 16-aligned here -- exactly the
    // call-site requirement, so (unlike fault.rs) no sub rsp,8 is needed.
    call timer_tick     // rax = kernel rsp to resume (this frame, or another)
    mov rsp, rax
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

.global sched_resume
sched_resume:
    // rdi = kernel rsp of a TrapFrame. One-way restore: load the GP set and
    // iretq into that context. Used for exit-driven switches and first run.
    mov rsp, rdi
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

.global sched_start
sched_start:
    // rdi = first process's kernel rsp. Save the kernel anchor (callee-saved
    // + rsp) so sched_return_to_kernel can come back here, then enter the
    // first process via the shared restore. Mirrors enter_user_asm.
    // gs:[{sched_anchor}] is PerCpu::sched_anchor (percpu.rs) -- per-core
    // (Stage B2.3, D6) so two cores each driving their own claim loop don't
    // share an anchor; GS_BASE points at THIS core's slot (percpu::init),
    // set up before either of these is ever reached.
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    mov gs:[{sched_anchor}], rsp
    jmp sched_resume    // rdi already holds the first kernel rsp

.global sched_return_to_kernel
sched_return_to_kernel:
    // rdi = value sched_start returns. Restore the anchor saved above and
    // return to sched_start's caller as if it returned normally. Mirrors
    // kernel_resume.
    mov rsp, gs:[{sched_anchor}]
    mov rax, rdi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    ret
"#,
    sched_anchor = const percpu::SCHED_ANCHOR_OFFSET,
);

extern "C" {
    fn timer_entry();
    /// Restore `kernel_rsp`'s context and iretq into it. Never returns.
    fn sched_resume(kernel_rsp: u64) -> !;
    /// Enter the first process; returns (with the last exit value) once every
    /// process has exited.
    fn sched_start(first_kernel_rsp: u64) -> u64;
    /// Abandon the current context and return from `sched_start`. Never
    /// returns to the caller.
    fn sched_return_to_kernel(value: u64) -> !;
}

/// Count of successful work-steals (a process moved from one core's array to
/// another's, `try_steal`). Monotonic for the life of the boot; the S4 demo
/// reads a before/after delta to prove a steal actually fired during it (the
/// one fact only stealing produces -- `Design/smp_scaling.md` section 6).
static STEAL_COUNT: AtomicU64 = AtomicU64::new(0);

/// Is the preemptive scheduler currently driving execution?
pub fn active() -> bool {
    SCHEDULER_ACTIVE.load(Ordering::Relaxed)
}

/// Total successful work-steals since boot (see `STEAL_COUNT`).
pub fn steals() -> u64 {
    STEAL_COUNT.load(Ordering::Relaxed)
}

/// Install the IRQ0 (timer) vector. Called while the IDT is built; the timer
/// is armed and fires only later, and only in ring 3.
pub fn register(idt: &mut InterruptDescriptorTable) {
    // SAFETY: timer_entry is the naked stub above; it hand-manages the
    // CPU-pushed frame and tail-resumes via iretq. set_handler_addr installs
    // an interrupt gate (IF cleared on entry) -- the basis of the
    // non-preemptible kernel.
    unsafe {
        idt[timer::TIMER_VECTOR]
            .set_handler_addr(VirtAddr::new(timer_entry as *const () as u64));
    }
}

/// Snapshot core `me`'s own queue as a state array for `pick_next` (kept pure
/// for testing): position `i` is `TABLE[CORE_QUEUE[me][i]].state`, or `Empty`
/// if that position holds no process. Replaces the old `states`/`states_for`
/// pair (S1): there is no "every slot, then mask out other cores'" scan any
/// more -- a core's queue holds only its own processes by construction.
fn core_states(me: u32) -> [State; MAX_PROCESSES] {
    let mut s = [State::Empty; MAX_PROCESSES];
    // SAFETY: scalar reads of this core's own queue and the single-CPU table;
    // the BKL is held by every caller.
    unsafe {
        let queue = &(*addr_of!(CORE_QUEUE))[me as usize];
        let table = &*addr_of!(TABLE);
        for (i, slot) in queue.iter().enumerate() {
            if let Some(t) = slot {
                s[i] = table[*t].state;
            }
        }
    }
    s
}

/// Position of `table_slot` within `me`'s own queue. `None` means `me` is not
/// actually `table_slot`'s home core -- every caller already knows it is
/// (each only ever asks about a process it just took off its own queue or
/// `CURRENT_SLOT`), so a `None` here would be a logic error, not a normal
/// case.
fn core_position(me: u32, table_slot: usize) -> Option<usize> {
    // SAFETY: scalar read of this core's own queue; the BKL is held by every
    // caller.
    unsafe { (*addr_of!(CORE_QUEUE))[me as usize].iter().position(|&s| s == Some(table_slot)) }
}

/// The `TABLE` slot at position `pos` in `me`'s own queue. Panics if `pos` is
/// empty -- every caller only ever passes a position `pick_next`/`core_states`
/// just reported as `Ready`, which can only be true if a process is there.
fn core_table_slot(me: u32, pos: usize) -> usize {
    // SAFETY: scalar read of this core's own queue; the BKL is held by every
    // caller.
    unsafe {
        (*addr_of!(CORE_QUEUE))[me as usize][pos]
            .expect("core_table_slot: position reported Ready but queue slot is empty")
    }
}

/// Insert `table_slot` into `core`'s queue at its first empty position.
/// Called once, from `setup_process`, when a process is homed. Always
/// succeeds: every core's queue is sized `MAX_PROCESSES`, the total number of
/// `TABLE` slots that can ever exist, so even every process landing on one
/// core fits.
fn push_to_core_queue(core: u32, table_slot: usize) {
    // SAFETY: scalar read/write of `core`'s own queue; the BKL is held by
    // every caller (`setup_process`'s caller holds it across the whole call).
    unsafe {
        let queue = &mut (*addr_of_mut!(CORE_QUEUE))[core as usize];
        let free = queue
            .iter()
            .position(|s| s.is_none())
            .expect("push_to_core_queue: core queue full (cannot exceed MAX_PROCESSES total)");
        queue[free] = Some(table_slot);
    }
}

/// Remove `table_slot` from `core`'s queue. Called once, from `on_exit`, at
/// the same point the slot itself is reset to `Slot::empty()` -- a dead entry
/// left in the queue would collide with a future process homed to the same
/// core once `TABLE` recycles the slot index.
fn remove_from_core_queue(core: u32, table_slot: usize) {
    // SAFETY: as `push_to_core_queue`.
    unsafe {
        let queue = &mut (*addr_of_mut!(CORE_QUEUE))[core as usize];
        let pos = queue
            .iter()
            .position(|&s| s == Some(table_slot))
            .expect("remove_from_core_queue: table_slot not found in its home core's queue");
        queue[pos] = None;
    }
}

/// The next online core in round-robin order (`NEXT_HOME_CORE`), skipping any
/// core id the MADT never brought up under this boot's `-smp` count. Always
/// terminates within `percpu::MAX_CORES` steps: core 0 (the BSP) is online
/// from boot, so the loop can never spin forever even under `-smp 1`.
fn next_home_core() -> u32 {
    // SAFETY: scalar read/write of a single counter; the BKL is held by every
    // caller (`setup_process`'s callers hold it across the whole call).
    unsafe {
        loop {
            let candidate = *addr_of!(NEXT_HOME_CORE) % percpu::MAX_CORES as u32;
            *addr_of_mut!(NEXT_HOME_CORE) = candidate + 1;
            if crate::irq::is_core_online(candidate as usize) {
                return candidate;
            }
        }
    }
}

/// S2 (work stealing): when `me`'s own queue has nothing Ready, look for a
/// Ready process homed to some OTHER core and move it to `me`'s queue,
/// returning its `TABLE` slot. Fixed scan order starting just after `me` and
/// wrapping through every other core id (mirroring `pick_next`'s own
/// from-current round-robin, applied across cores instead of within one);
/// the first Ready entry found anywhere is taken -- no priority, no load
/// metric beyond "is anything Ready at all." Steals at most one slot: the
/// caller resumes immediately on success (`resume_process` never returns),
/// so there is never a second steal attempt within the same idle check.
/// `None` if no other core's queue has anything Ready right now.
///
/// This is the one place a process's home core actually changes after
/// setup -- safe per the S3 re-audit above: a Ready process is by
/// construction not mid-syscall on its donor core (D5's non-preemptible-
/// kernel invariant), and the whole move happens under the BKL, so no other
/// core can observe the slot mid-transfer.
///
/// Steal-eligibility (S2 correctness, Bug C fix 2026-06-24): a slot is taken
/// only if it is Ready AND it is NOT the donor core's `CURRENT_SLOT`. The
/// `CURRENT_SLOT` exclusion is load-bearing. A core that blocks its process
/// (`block_current`) leaves `CURRENT_SLOT[donor]` pointing at it and keeps it
/// in the donor's queue while waiting for a peer's `wake_with` to flip it back
/// to Ready -- the core is specifically waiting to RESUME that exact process
/// (`switch_to_next`'s `states[cur_pos] == Ready` check). Without this guard,
/// another core's `try_steal` could grab it in the brief Ready window between
/// `wake_with` and the home core reclaiming it: the home core's `CURRENT_SLOT`
/// would then dangle (no longer in its own `CORE_QUEUE`), breaking the
/// `CURRENT_SLOT[me] in CORE_QUEUE[me]` invariant (`block_current`'s
/// `core_position().expect()`), stranding the IPC rendezvous, and hanging
/// `smoke-smp`. Because a slot lives in exactly one core's queue, the donor's
/// own `CURRENT_SLOT` is the only one that can ever name a slot in its queue,
/// so this single comparison is the whole guard. Legitimately stealable work
/// is untouched: a timer-preempted process (Ready, but `CURRENT_SLOT` already
/// moved to its successor) and a never-started queued process (Ready, never
/// any core's current) both remain fair game -- the imbalance the S4 demo
/// creates still resolves by stealing.
fn try_steal(me: u32) -> Option<usize> {
    for offset in 1..percpu::MAX_CORES as u32 {
        let donor = (me + offset) % percpu::MAX_CORES as u32;
        // SAFETY: scalar reads of the donor's queue, its CURRENT_SLOT, and the
        // single-CPU table; the BKL is held by every caller (CURRENT_SLOT is
        // only ever written under the BKL, so this read is not torn).
        let stolen = unsafe {
            let donor_current = (*addr_of!(CURRENT_SLOT))[donor as usize];
            (*addr_of!(CORE_QUEUE))[donor as usize].iter().find_map(|slot| {
                let t = (*slot)?;
                (t != donor_current && (*addr_of!(TABLE))[t].state == State::Ready).then_some(t)
            })
        };
        if let Some(table_slot) = stolen {
            remove_from_core_queue(donor, table_slot);
            push_to_core_queue(me, table_slot);
            STEAL_COUNT.fetch_add(1, Ordering::Relaxed);
            return Some(table_slot);
        }
    }
    None
}

/// Round-robin policy: the next `Ready` slot strictly after `current`
/// (wrapping), or `None` if no other process is runnable. Pure -- unit-tested
/// without any hardware (see tests/scheduler.rs).
pub fn pick_next(slots: &[State; MAX_PROCESSES], current: usize) -> Option<usize> {
    // Visit every OTHER slot once, in round-robin order; never `current`
    // itself (a process does not pick itself -- it keeps running when no one
    // else is Ready).
    for offset in 1..MAX_PROCESSES {
        let i = (current + offset) % MAX_PROCESSES;
        if slots[i] == State::Ready {
            return Some(i);
        }
    }
    None
}

/// Account one tick against the quantum; return whether it expired (and reset
/// it if so).
fn quantum_expired() -> bool {
    // SAFETY: this core's own slot (percpu::core_id); reached only from its
    // own timer handler (IF=0), never aliased by another core.
    unsafe {
        let slot = &mut (*addr_of_mut!(TICKS_IN_QUANTUM))[percpu::core_id()];
        *slot += 1;
        if *slot >= QUANTUM {
            *slot = 0;
            true
        } else {
            false
        }
    }
}

/// The timer interrupt handler body (reached from the `timer_entry` stub).
/// Returns the kernel rsp the stub should resume: the same frame (no switch)
/// or the next process's saved frame (switch).
#[no_mangle]
extern "C" fn timer_tick(frame: *const TrapFrame) -> u64 {
    // BKL (D4): this function never diverges -- every path below is a
    // normal Rust return, and the stub's iretq runs only after we return --
    // so acquiring once at the top and releasing before each return
    // correctly brackets the whole body (see bkl.rs).
    bkl::acquire();

    timer::note_tick();
    crate::irq::eoi(0); // acknowledge IRQ0 at the interrupt controller

    // SAFETY: the stub passes a pointer to the TrapFrame it built on the
    // current kernel stack; valid for this call.
    let from_user = unsafe { (*frame).cs & 3 == 3 };

    // Non-preemptible kernel: only reschedule out of ring 3, and only when the
    // quantum is up. (Counting the quantum regardless of CPL is fine; kernel
    // ticks are rare and never switch.)
    //
    // Also bail if the preemptive scheduler is not actually driving anything
    // right now (the older synchronous single-process demos -- hello, bump,
    // etc. -- still run via usermode::enter_user, not scheduler::run): with
    // the flat table (pre-S1), an idle TABLE just made pick_next return None
    // harmlessly; with a real per-core array, `cur` has no meaning at all
    // when nothing was ever homed, so this must be checked explicitly rather
    // than relying on an empty queue to no-op the same way an empty table did.
    if !from_user || !quantum_expired() || !active() {
        unsafe { bkl::release() };
        return frame as u64;
    }

    let me = unsafe { percpu::core_id() as u32 };
    let cur = unsafe { (*addr_of!(CURRENT_SLOT))[me as usize] };
    // S1 (real per-core array): pick_next only ever sees this core's own
    // queue, so there is no separate "filter out other cores' work" step --
    // a process homed elsewhere is simply not in `core_states(me)` at all.
    let cur_pos = core_position(me, cur)
        .expect("timer_tick: running process missing from its own core queue");
    let Some(next_pos) = pick_next(&core_states(me), cur_pos) else {
        // Nobody else is runnable (e.g. the synchronous demos, where the
        // table is empty): keep running this process. This is exactly the
        // Stage-1 "count and return" behavior.
        unsafe { bkl::release() };
        return frame as u64;
    };
    let next = core_table_slot(me, next_pos);

    // Suspend the running process into its slot, with its saved context.
    // SAFETY: the BKL (D4) is held; CURRENT holds the running process and no
    // other lock is held across the return (the stub does the iretq after we
    // return).
    unsafe {
        let running = process::current()
            .lock()
            .take()
            .expect("CURRENT vanished under the scheduler");
        let table = &mut *addr_of_mut!(TABLE);
        table[cur].kernel_rsp = frame as u64;
        table[cur].process = Some(running);
        table[cur].state = State::Ready;

        // Resume the next process.
        let nproc = table[next]
            .process
            .take()
            .expect("Ready slot has no process");
        table[next].state = State::Running;
        let next_rsp = table[next].kernel_rsp;
        let next_l4 = nproc.l4;
        *process::current().lock() = Some(nproc);
        (*addr_of_mut!(CURRENT_SLOT))[me as usize] = next;
        memory::switch_to(next_l4);
        gdt::set_kernel_stack(kstack_top(next));
        bkl::release();
        next_rsp
    }
}

/// Build a process from `binary` into slot `id`: a private address space, the
/// loaded image, a fresh `Process`, and a fabricated initial trap frame on the
/// slot's kernel stack (with rdi = id, so `_start` receives its id). Leaves
/// the slot Ready, homed to `home_core`. Reuses the same loader the
/// synchronous path uses.
fn setup_process(
    id: usize,
    binary: &[u8],
    phys_offset: u64,
    caps: &[Option<Capability>],
    notify: bool,
    home_core: u32,
) -> Result<(), &'static str> {
    let l4 = memory::create_address_space()?;
    let mut boot_frames: [Option<(u64, u64)>; MAX_BOOT_FRAMES] = [None; MAX_BOOT_FRAMES];
    let entry = match process::load_and_map(binary, phys_offset, l4, &mut boot_frames) {
        Ok(entry) => entry,
        Err(e) => {
            memory::destroy_address_space(l4);
            return Err(e);
        }
    };

    let mut proc = process::spawn_process(None);
    // Grants (e.g. endpoint capabilities) land right after the CPU-time budget,
    // in mint order, so each is at a well-known slot (the first at
    // ENDPOINT_SLOT/GRANT_SLOT, the next after, ...).
    for cap in caps.iter().flatten() {
        if proc.caps.mint(cap.object, cap.rights).is_err() {
            memory::destroy_address_space(l4);
            return Err("capability table full");
        }
        // Account an endpoint grant entering the new table (no-op otherwise).
        // This is the single mint site for boot demo grants, the spawn child's
        // send cap, and the receiving half of a spawn capability transfer.
        ipc::note_cap_added(cap);
    }
    proc.l4 = l4;

    // SAFETY: id < MAX_PROCESSES (checked by the caller); the kernel stack and
    // table slot are this process's own.
    let kernel_rsp = unsafe { fabricate_initial_frame(id, entry) };
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        table[id].process = Some(proc);
        table[id].boot_frames = boot_frames;
        table[id].kernel_rsp = kernel_rsp;
        table[id].state = State::Ready;
    }
    push_to_core_queue(home_core, id);
    // The new entry sits in `home_core`'s queue, but that core may be halted
    // in `ap_idle_loop`/`idle_until_runnable` waiting for any wake at all --
    // it has no reason to re-check its own queue until something nudges it.
    // Wake every online core out of `hlt` rather than targeting `home_core`
    // alone: simple, and an extra core waking to find nothing of its own
    // Ready is harmless. The caller (run(), spawn()) holds the BKL across
    // this whole function, same as every other TABLE write. `run()` sets up
    // several processes per call; its caller sends one IPI after the whole
    // batch instead of one per process (notify=false here) to avoid issuing
    // several ICR sends back to back with no gap.
    if notify {
        crate::irq::send_reschedule_ipi();
    }
    Ok(())
}

/// Lay down an initial TrapFrame at the top of slot `id`'s kernel stack so the
/// first `sched_resume` iretqs into `entry` in ring 3 on a fresh user stack,
/// with rdi = id. First run and a resume from preemption are then the same
/// code path. Returns the kernel rsp pointing at the frame's base.
///
/// # Safety
/// `id` must be < MAX_PROCESSES.
unsafe fn fabricate_initial_frame(id: usize, entry: u64) -> u64 {
    let base = kstack_top(id) - core::mem::size_of::<TrapFrame>() as u64;
    let frame = &mut *(base as *mut TrapFrame);
    frame.gp = [0; 15];
    frame.gp[GP_RDI] = id as u64;
    frame.rip = entry;
    frame.cs = 0x23; // user code selector (matches enter_user_asm / the GDT)
    frame.rflags = 0x202; // reserved bit + IF: ring 3 runs with interrupts on
    frame.rsp = USER_STACK_TOP;
    frame.ss = 0x1b; // user data selector
    base
}

/// Launch each binary as an independent process and round-robin them under the
/// timer until all have exited. Returns to the boot path when the last one
/// exits. Per design D4 these are kernel-launched, independent processes; this
/// does not use (or compose with) `spawn`.
pub fn run(label: &str, binaries: &[&[u8]], phys_offset: u64, extra: &[Option<Capability>]) {
    let mut serial = serial::init();
    let count = binaries.len().min(MAX_PROCESSES);
    let _ = writeln!(serial, "plinth: {label}: {count} processes");

    // BKL (D4): `run` is boot-driving code, not a dispatch body, so nothing
    // holds the lock when it is called -- it acquires its own, covering the
    // setup loop and the inline slot-0 claim below (both touch TABLE, which
    // an AP's claim loop may be reading/writing concurrently, Stage B2.3),
    // and releases it right before sched_start hands off to ring 3.
    bkl::acquire();

    // SAFETY: `run` is BSP-only (see IS_LAUNCHER), so `me` is stable for the
    // rest of this call.
    let me = unsafe { percpu::core_id() as u32 };

    for (id, binary) in binaries.iter().take(count).enumerate() {
        // Grant assignment: a SINGLE-process run hands that process EVERY entry in
        // `extra` (so a demo can be given several capabilities -- the unified
        // block+input loop needs a BlockRange and an EventSource, which mint into
        // consecutive slots after the CPU budget). A MULTI-process run is one grant
        // per process, `extra[id]` (the common case: an endpoint per peer, a
        // BlockRange per reader). `setup_process` mints the whole slice in order.
        let grants: &[Option<Capability>] = if count == 1 {
            extra
        } else {
            extra.get(id).map(core::slice::from_ref).unwrap_or(&[])
        };
        // S1 (real per-core array): slot 0 is always homed to the launching
        // core (always the BSP -- `run` is BSP-only) so the inline claim
        // below needs no separate insert of its own; every other slot is
        // distributed round-robin over whatever cores are actually online
        // this boot.
        let home_core = if id == 0 { me } else { next_home_core() };
        if let Err(e) = setup_process(id, binary, phys_offset, grants, false, home_core) {
            let _ = writeln!(serial, "plinth: {label}: setup of process {id} failed: {e}");
            // Reclaim whatever was set up before aborting the demo.
            teardown_all();
            unsafe { bkl::release() };
            return;
        }
    }
    // One IPI for the whole batch (not one per process, Stage B2.3): slots
    // 1..count may be homed to an AP that is currently halted with nothing
    // of its own yet to wake for; slot 0 is about to be claimed inline below
    // on this core, so it needs no IPI of its own.
    if count > 1 {
        crate::irq::send_reschedule_ipi();
    }

    SCHEDULER_ACTIVE.store(true, Ordering::Relaxed);

    // Install the first process as CURRENT and enter it. sched_start returns
    // only once every process has exited (via on_exit -> sched_return_to_kernel).
    // SAFETY: slot 0 was just set up Ready (homed to `me` above; no separate
    // claim needed); the BKL is held, IF=0 here.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        let proc = table[0].process.take().expect("first slot has no process");
        table[0].state = State::Running;
        let rsp = table[0].kernel_rsp;
        let l4 = proc.l4;
        *process::current().lock() = Some(proc);
        (*addr_of_mut!(CURRENT_SLOT))[me as usize] = 0;
        // This core now has a valid sched_anchor (sched_start sets it next) --
        // record it once, so on_exit/switch_to_next can tell this core apart
        // from an AP that has none to longjmp back through.
        (*addr_of_mut!(IS_LAUNCHER))[me as usize] = true;
        memory::switch_to(l4);
        gdt::set_kernel_stack(kstack_top(0));
        bkl::release();
        sched_start(rsp);
    }

    // Back on the boot kernel stack; the last process already returned us to
    // the kernel address space in on_exit, but be explicit.
    memory::switch_to_kernel();
    SCHEDULER_ACTIVE.store(false, Ordering::Relaxed);
    let _ = writeln!(serial, "plinth: {label}: all done");
}

/// Reclaim the process currently on the CPU and continue: switch to the next
/// runnable process, or, if none remain, return to `run`. Reached from
/// `process::exit_current` for every scheduled-process death (exit, fault,
/// budget overdraw). Never returns to its caller.
pub fn on_exit() -> ! {
    let me = unsafe { percpu::core_id() as u32 };
    let cur = unsafe { (*addr_of!(CURRENT_SLOT))[me as usize] };
    // Captured before `remove_from_core_queue` below removes this slot from
    // the queue entirely -- `switch_to_next` still needs a position to anchor
    // its round-robin scan from, even though the position itself is about to
    // go back to empty.
    let cur_pos =
        core_position(me, cur).expect("on_exit: exiting process missing from its own core queue");

    // Take the dying process out of CURRENT and reclaim it. Move to the kernel
    // address space first so destroy_address_space tears down tables that are
    // no longer the active CR3.
    let proc = process::current()
        .lock()
        .take()
        .expect("no CURRENT process at exit");
    memory::switch_to_kernel();
    // Wake any live peer this death would otherwise strand -- a caller awaiting
    // a reply this process owed, or a process blocked on an endpoint whose last
    // counterpart this was. Runs BEFORE teardown drains the caps, so the reply
    // targets and endpoint queues are still intact; teardown then applies the
    // refcount decrements and frees the slot (hardening D5).
    ipc::reap_dying(&proc.caps);
    let boot_frames = unsafe { (*addr_of!(TABLE))[cur].boot_frames };
    let l4 = proc.l4;
    process::teardown(proc, &boot_frames);
    memory::destroy_address_space(l4);
    // SAFETY: single CPU, IF=0; the slot is the dying process's own. A dying
    // process is never itself an enqueued waiter (death only hits Running), so
    // clearing its link is hygiene, not correctness -- enqueue rewrites the
    // link before any reuse anyway.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        table[cur] = Slot::empty();
        (*addr_of_mut!(WAIT_LINKS))[cur] = None;
        remove_from_core_queue(me, cur);
    }

    // The current process is gone; switch_to_next decides what "nothing else
    // claimable" means from here (launcher vs. AP, table empty vs. other
    // cores' work still live -- see NoWorkAction's doc).
    unsafe { switch_to_next(cur_pos, NoWorkAction::ExitedReturnIfDone) }
}

/// True once `run` has called `sched_start` on this core, i.e. this core has
/// a valid `sched_anchor` to longjmp back to via `sched_return_to_kernel`
/// (Stage B2.3). Only ever the BSP today -- `run` is BSP-only -- but tracked
/// per-core, set once and never cleared, rather than hardcoded: this is what
/// `switch_to_next` checks to tell "the core driving `run`, with everything
/// now exited" apart from "an AP whose own current process just exited, with
/// nothing else for it to claim right now" -- the latter has no anchor to
/// return to and must keep idling instead (a real bug found by booting this:
/// an AP that wrongly took the `Return` path longjmped through its unset
/// (zero) anchor and corrupted its stack).
static mut IS_LAUNCHER: [bool; percpu::MAX_CORES] = [false; percpu::MAX_CORES];

/// Why this `switch_to_next` call exists -- reached from `on_exit` (this
/// core's own process is gone for good) or `block_current` (it is merely
/// waiting on a specific peer, expected back very soon). Once the loop finds
/// nothing locally and nothing to steal, both cases converge on the same
/// outcome once the table is truly empty (see the `table_entirely_empty`
/// branch below) -- the only thing `NoWorkAction` still distinguishes is
/// whether stealing is appropriate at all (S2: gated to `ExitedReturnIfDone`
/// only, see the comment at the steal call site for why).
///
/// S1/S2 bug found by booting this: an earlier version of this enum gave
/// `BlockedDeadlockIfNoOtherWork` its own table-empty branch that panicked,
/// on the reasoning "if literally everything is Empty, my block can never be
/// satisfied." That reasoning held before stealing existed (a genuinely
/// Blocked process is never Empty, so `table_entirely_empty` could never
/// observe a real deadlock anyway -- it would just hang in the wait loop
/// below, forever, which is a separate, pre-existing limitation). Once
/// stealing exists, `cur_pos` can be taken by another core's `try_steal`
/// while this call is still waiting on it; that other core then resumes,
/// finishes, and exits the very process this call was waiting for -- so
/// `table_entirely_empty` becoming true here means "my own wait was already
/// resolved, just not by me," not "deadlock." Treating it identically to the
/// exited case (idle, or return if launcher) is correct either way.
enum NoWorkAction {
    /// Reached from `on_exit`: this core's own process is gone, no
    /// expectations left, free to steal.
    ExitedReturnIfDone,
    /// Reached from `block_current`: this core's own process expects a
    /// specific peer to wake it soon. Not free to steal (S2) -- but if the
    /// table empties out anyway, that peer (or whoever it became, after a
    /// steal elsewhere) already resolved this wait.
    BlockedDeadlockIfNoOtherWork,
}

/// True if every slot is Empty -- nothing alive anywhere, on any core, used
/// to tell a genuine system-wide deadlock/completion apart from "just
/// nothing claimable by ME right now" (Stage B2.3: another core's live
/// process may still resolve the situation).
fn table_entirely_empty() -> bool {
    // SAFETY: scalar reads of the table; the BKL is held by every caller.
    unsafe { (*addr_of!(TABLE)).iter().all(|s| s.state == State::Empty) }
}

/// Pick the next Ready process this core may claim and resume it, or handle
/// the no-work case per `on_idle` (see `NoWorkAction`). Shared by `on_exit`
/// and `block_current`; never returns.
///
/// `cur_pos` is the leaving process's position in this core's own queue,
/// captured by the caller before this call -- not re-derived here, because
/// `on_exit` has already removed the leaving process from the queue entirely
/// by the time it calls in (S1: a dead entry must not survive to collide with
/// a future process homed to the same slot index), so a fresh `core_position`
/// lookup would find nothing. `cur_pos` remains valid as a round-robin anchor
/// either way -- `pick_next` only needs a numeric position to start scanning
/// from, not a populated one. The function never takes the leaving process's
/// `TABLE` slot itself as a parameter (an earlier version did, see the S1 bug
/// note below): this call's own hlt-wait loop can persist across many later,
/// unrelated demos if `table_entirely_empty` never coincides with one of its
/// wakeups, and by then `cur_pos` may have been legitimately recycled for a
/// brand-new process homed to this same core -- a frozen `TABLE` slot
/// captured at call time would silently go stale, while always resolving
/// through `core_table_slot(me, cur_pos)` fresh stays correct either way.
///
/// # Safety
/// Caller must have already removed the leaving process from `CURRENT` (taken
/// it for teardown, or parked it in its slot as Blocked), must hold the BKL
/// (D4), and must hold no other locks.
unsafe fn switch_to_next(cur_pos: usize, on_idle: NoWorkAction) -> ! {
    let me = percpu::core_id() as u32;
    loop {
        // S1 (real per-core array): `core_states(me)` is already scoped to
        // this core's own queue -- no separate ownership filter needed.
        let states = core_states(me);
        if let Some(next_pos) = pick_next(&states, cur_pos) {
            resume_process(core_table_slot(me, next_pos)); // never returns
        }

        // pick_next deliberately never returns `cur` itself (round-robin
        // fairness when there IS other Ready work). But the process that
        // just left `cur` -- blocked, not exited -- can be woken right back
        // to Ready by a peer's `wake_with` before we even reach here, or
        // while we wait below, and on a different core that peer may be the
        // only thing that will ever wake us (Stage B2.3: a bug found by
        // booting this -- a process blocked on a peer owned by another core
        // hung forever, because this case fell through to the generic wait
        // below and every later retry kept asking pick_next, which kept
        // skipping `cur`). `on_exit` already emptied `cur`'s slot (and
        // removed it from the queue) before calling here, so this is
        // harmless (always false, since `core_states` maps the now-empty
        // `cur_pos` to `State::Empty`) on that path.
        //
        // S1 bug found by booting this (real per-core array): resume
        // whatever `core_table_slot` reports for `cur_pos` right now, NOT
        // the originally-captured `cur`. This call can sit in its hlt-wait
        // loop across many LATER demos if `table_entirely_empty` never
        // coincides with one of its wakeups (no IPI fires at the exact
        // instant the table is briefly all-empty between two demos) -- an
        // AP can stay here, with `cur`/`cur_pos` frozen, indefinitely.
        // `cur_pos` is just a queue POSITION; a later demo's `setup_process`
        // can legitimately home a brand-new process to this same core at
        // that exact recycled position, and `core_states` (re-read fresh
        // every loop iteration) will correctly show it Ready -- but `cur`
        // (the table slot identity captured back when THIS call started)
        // no longer names what is actually sitting there. Resuming `cur`
        // directly would resume an empty slot (no process); resuming
        // whatever `core_table_slot(me, cur_pos)` names now is correct
        // whether or not the position's occupant ever changed.
        if states[cur_pos] == State::Ready {
            resume_process(core_table_slot(me, cur_pos)); // never returns
        }

        // S2: an AP (not the launcher) whose own process just exited has no
        // expectation left at all -- hand off to `ap_idle_loop` immediately
        // rather than looping here. `ap_idle_loop` already implements its
        // own "steal, halt, recheck" cycle; an earlier version of this
        // function ran a SECOND, nearly-identical steal-then-wait cycle
        // right here too, so a core could have two independent loops
        // watching (and racing on) the same `CORE_QUEUE`/`TABLE` state at
        // once -- one inside this still-live `switch_to_next` frame, one
        // entered later through a fresh `ap_idle_loop` call. S2 bug found
        // by booting this: that duplication produced a rare,
        // timing-dependent hang (only reproduced intermittently, never on
        // the same demo twice) that disappeared once the two loops were
        // collapsed into one. The launcher cannot take this exit -- it has
        // no anchor to return through except via this same call, gated on
        // `table_entirely_empty` below -- so it still tries a single steal
        // here first (harmless either way) before falling into that wait.
        //
        // `block_current`'s caller is unaffected either way: it expects ITS
        // OWN position to come back Ready very soon (the peer it is waiting
        // on, often already running on another core) -- an earlier S1 bug
        // found by booting this showed that stealing on the blocked path
        // too thrashed both processes back and forth on every IPC round
        // trip, so it stays excluded from stealing entirely.
        if matches!(on_idle, NoWorkAction::ExitedReturnIfDone) {
            if !IS_LAUNCHER[me as usize] {
                bkl::release();
                ap_idle_loop(); // never returns
            }
            if let Some(stolen) = try_steal(me) {
                resume_process(stolen); // never returns
            }
        }

        // Nothing claimable BY ME, nothing to steal either. A process blocked
        // on EXTERNAL input or on disk I/O is not a deadlock -- a keystroke or
        // a virtio completion IRQ can still arrive -- so idle and wait for it
        // rather than treat it as stuck.
        let input_waiter = crate::input::any_waiter();
        if input_waiter || crate::virtio_blk::any_waiter() {
            // Deterministic delivery for headless smoke: if a synthetic
            // keyboard event is armed, deliver it now (it wakes the blocked
            // reader). Real keystrokes -- and every disk completion -- arrive
            // via their device IRQ during the idle below.
            if input_waiter {
                crate::input::deliver_synthetic();
                crate::input::deliver_synthetic_mouse();
            }
            idle_until_runnable(); // never returns
        }

        if table_entirely_empty() {
            // S1/S2 (see NoWorkAction's doc): table-empty means "nothing
            // left for me, whether my own process exited here or was
            // resolved elsewhere after a steal" -- `on_idle` no longer
            // distinguishes an outcome at this point, only `IS_LAUNCHER`
            // does.
            if IS_LAUNCHER[me as usize] {
                // SAFETY: sched_start saved the anchor before entering the
                // first process, and its stack frame is still live. BKL
                // (D4): returning to `run`'s caller, ordinary (non-dispatch)
                // boot-sequence code -- release before this longjmp, the
                // same as the ring-3 chokepoint in resume_process.
                bkl::release();
                sched_return_to_kernel(0); // never returns
            } else {
                // Not the launcher -- no anchor to return to, and the table
                // really is empty, so there is nothing to claim either;
                // park exactly like any other idle core.
                bkl::release();
                ap_idle_loop(); // never returns
            }
        }

        // Other cores still have live work (Stage B2.3): not (yet) a
        // deadlock and not done -- wait for a wake (reschedule IPI or device
        // IRQ) and recheck from the top, the same idle discipline as
        // `idle_until_runnable`, just without that path's precondition of
        // already having a blocked input/disk waiter to justify the wait.
        bkl::release();
        x86_64::instructions::interrupts::enable_and_hlt();
        x86_64::instructions::interrupts::disable();
        bkl::acquire();
    }
}

/// Resume the Ready process at `next`: make it Running, switch to its address
/// space and kernel stack, and `sched_resume` into its saved trap frame. Never
/// returns. Shared by the normal pick and the idle-on-input loop.
///
/// # Safety
/// `next` must name a Ready slot holding a process; the caller holds the BKL
/// and no other locks.
unsafe fn resume_process(next: usize) -> ! {
    let me = percpu::core_id() as u32;
    let table = &mut *addr_of_mut!(TABLE);
    // S1 (real per-core array): `next` is already homed to `me`'s own queue
    // (every caller selects it from `core_states(me)`/`first_claimable_slot`,
    // both scoped to this core), so there is no claim step here any more.
    let nproc = table[next].process.take().expect("Ready slot has no process");
    table[next].state = State::Running;
    let rsp = table[next].kernel_rsp;
    let l4 = nproc.l4;
    *process::current().lock() = Some(nproc);
    CURRENT_SLOT[me as usize] = next;
    memory::switch_to(l4);
    gdt::set_kernel_stack(kstack_top(next));
    // BKL (D4): about to iretq into ring 3 via sched_resume, which never
    // returns and never runs Drop -- release here, the one chokepoint every
    // "switch to a Ready process" path (on_exit, block_current,
    // idle_until_runnable, timer_tick's own inline switch is separate) funnels
    // through.
    bkl::release();
    sched_resume(rsp)
}

/// Idle with interrupts enabled until a device IRQ makes a blocked process
/// Ready, then resume it. Entered only when a process is blocked on input or
/// disk I/O and nothing else runs: the keyboard IRQ delivers a real event
/// (waking a reader), or the virtio completion IRQ wakes a process blocked on
/// `block_read`; the Stage-2 synthetic keyboard injection is delivered by the
/// caller before we get here. Never returns.
///
/// # Safety
/// The caller holds the BKL (D4) and no other locks; a process is blocked on
/// input, so a wake is possible (otherwise this idles forever, which is
/// correct -- the system is waiting for a keystroke).
unsafe fn idle_until_runnable() -> ! {
    let me = percpu::core_id() as u32;
    loop {
        // A delivery (synthetic, or a keyboard IRQ on a prior iteration) may have
        // made a reader Ready -- including the one in CURRENT_SLOT, which
        // pick_next skips, so scan this core's own queue for any Ready slot.
        if let Some(next) = first_claimable_slot(me) {
            resume_process(next);
        }
        // S2: nothing of mine is Ready yet either -- try stealing before
        // halting. The input/disk waiter this loop exists for is unaffected
        // either way: it is blocked on its own home core's queue (or wakes
        // via a device IRQ), not on this core doing anything in particular.
        if let Some(stolen) = try_steal(me) {
            resume_process(stolen);
        }
        // BKL (D4): the lock must NOT be held while halted -- a device IRQ's
        // handler (keyboard_interrupt, blk_interrupt_*) needs to acquire it to
        // record the event/completion that wakes us, and (from B2.3) another
        // core needs it to schedule. Release before the wait, re-acquire
        // before touching TABLE again on the next iteration.
        unsafe { bkl::release() };
        // Enable interrupts and halt as one atomic step (sti;hlt -- no wakeup
        // lost between them); a device IRQ wakes us, then we re-disable and
        // re-check.
        x86_64::instructions::interrupts::enable_and_hlt();
        x86_64::instructions::interrupts::disable();
        bkl::acquire();
    }
}

/// `TABLE` slot of the first Ready process in core `me`'s own queue. Unlike
/// `pick_next` (round-robin, which skips the current slot), this includes the
/// current slot -- the idle loop must resume a reader that blocked and was
/// just woken in place. Shared by `idle_until_runnable` (must already have a
/// reason to wait -- a blocked input/disk waiter) and `ap_idle_loop` (waits
/// for any claimable work at all).
fn first_claimable_slot(me: u32) -> Option<usize> {
    core_states(me)
        .iter()
        .position(|&s| s == State::Ready)
        .map(|pos| core_table_slot(me, pos))
}

// ---------------------------------------------------------------------------
// Blocking support for IPC (ipc.rs). The endpoint objects and the matching
// policy live in ipc.rs; the scheduler owns the process state transitions and
// the intrusive wait-queue links (which live in the slots).
// ---------------------------------------------------------------------------

/// Index of the process currently on THIS core.
pub fn current_slot() -> usize {
    // SAFETY: this core's own slot (percpu::core_id).
    unsafe { CURRENT_SLOT[percpu::core_id()] }
}

/// Run `f` with the wait-queue link array, so the IPC layer can drive its pure
/// `WaitQueue` over the real (single-CPU) link store. Reached only from IPC
/// dispatch (IF=0); `WAIT_LINKS` is distinct from every other static the IPC
/// path borrows, so this never aliases.
pub fn with_wait_links<R>(f: impl FnOnce(&mut [Option<usize>]) -> R) -> R {
    // SAFETY: single CPU, IF=0; the borrow lives only for the call to `f`.
    unsafe { f(&mut *addr_of_mut!(WAIT_LINKS)) }
}

/// Stash the message, cap-slot, and call-flag a blocked sender carries.
pub fn set_pending(slot: usize, msg: u64, cap: u64, is_call: bool) {
    // SAFETY: as above.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        table[slot].pending_msg = msg;
        table[slot].pending_cap = cap;
        table[slot].pending_call = is_call;
    }
}

pub fn take_pending(slot: usize) -> u64 {
    // SAFETY: as above.
    unsafe { (*addr_of!(TABLE))[slot].pending_msg }
}

pub fn take_pending_cap(slot: usize) -> u64 {
    // SAFETY: as above.
    unsafe { (*addr_of!(TABLE))[slot].pending_cap }
}

pub fn take_pending_call(slot: usize) -> bool {
    // SAFETY: as above.
    unsafe { (*addr_of!(TABLE))[slot].pending_call }
}

/// Is the process in `slot` Blocked? `reply` uses this to confirm the caller a
/// reply capability names is still awaiting its reply before waking it.
pub fn is_blocked(slot: usize) -> bool {
    // SAFETY: scalar read of the single-CPU table.
    unsafe { (*addr_of!(TABLE))[slot].state == State::Blocked }
}

/// Wake a Blocked process: make it Ready and write the IPC result into its
/// saved trap frame, so when the scheduler resumes it the blocking IPC call
/// returns these values. ABI v2 splits the result across three registers:
/// `status` in rax (IPC_OK / IPC_PEER_DIED / IPC_ERR), the message `payload` in
/// rsi, and the transferred cap's `landing` slot (or NO_CAP) in rdx. A receiver
/// reads all three; a woken sender reads only the status (its send wrapper
/// treats rsi/rdx as clobbered).
pub fn wake_with(slot: usize, status: u64, payload: u64, landing: u64) {
    // SAFETY: the slot is Blocked, so its kernel_rsp points at a valid saved
    // trap frame on its own (live) kernel stack; the BKL (D4) is held by
    // every caller.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        let frame = table[slot].kernel_rsp as *mut TrapFrame;
        (*frame).gp[GP_RAX] = status;
        (*frame).gp[GP_RSI] = payload;
        (*frame).gp[GP_RDX] = landing;
        table[slot].state = State::Ready;
    }
    // The slot was already homed to a core (only a Running process can have
    // blocked) -- its home core may be halted waiting for exactly this; wake
    // every other online core out of `hlt` to go check. Slightly wasteful
    // (only the home core cares) but simple and correct; targeted IPIs are a
    // B3-style optimization, not needed before there is evidence this
    // contends.
    crate::irq::send_reschedule_ipi();
}

/// Mint `cap` into the table of the Blocked process at `slot`; returns its
/// landing slot, or None if that process's capability table is full. Used by
/// the IPC layer to deliver a transferred capability into a blocked receiver.
pub fn mint_into_blocked(slot: usize, cap: Capability) -> Option<usize> {
    // SAFETY: single CPU, IF=0; the slot holds a Blocked process.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        table[slot]
            .process
            .as_mut()?
            .caps
            .mint(cap.object, cap.rights)
            .ok()
    }
}

/// Revoke (and, if a mapped frame, unmap) the capability at `cap_slot` from
/// the Blocked process at `slot`. Used when a receiver completes a rendezvous
/// with a blocked sender that is transferring a capability.
pub fn revoke_from_blocked(slot: usize, cap_slot: usize) -> Option<Capability> {
    // SAFETY: single CPU, IF=0; the slot holds a Blocked process.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        process::revoke_and_unmap(table[slot].process.as_mut()?, cap_slot)
    }
}

/// Park the current process as Blocked (saving the trap frame the IPC stub
/// built) and switch to the next runnable process. The caller (ipc.rs) must
/// have already enqueued the current slot on the endpoint it is waiting for.
/// Never returns to its caller; the process resumes later via `wake_with` +
/// the scheduler picking it up.
pub fn block_current(frame_ptr: u64) -> ! {
    // SAFETY: reached from the IPC interrupt handler with IF=0, the BKL (D4)
    // held (acquired by ipc_dispatch) and no other locks; CURRENT holds the
    // running process.
    unsafe {
        let me = percpu::core_id() as u32;
        let cur = CURRENT_SLOT[me as usize];
        let cur_pos = core_position(me, cur)
            .expect("block_current: blocking process missing from its own core queue");
        let running = process::current()
            .lock()
            .take()
            .expect("no CURRENT process to block");
        let table = &mut *addr_of_mut!(TABLE);
        table[cur].kernel_rsp = frame_ptr;
        table[cur].process = Some(running);
        table[cur].state = State::Blocked;
        switch_to_next(cur_pos, NoWorkAction::BlockedDeadlockIfNoOtherWork)
    }
}

/// Launch `binary` as a new scheduled process while the scheduler is running,
/// minting `caps` into it. Returns its slot, or None if the process table is
/// full or setup failed. The new process is Ready and joins the round-robin;
/// this does not switch to it. Used by `sys_spawn` to make spawn a scheduler
/// operation (the child runs alongside its parent) rather than synchronous
/// nesting. Homed to the spawning core (S1) -- a child starts out cache-warm
/// on whichever core spawned it; stealing (S2) is what may later move it.
pub fn spawn(binary: &[u8], phys_offset: u64, caps: &[Option<Capability>]) -> Option<usize> {
    let id = find_free_slot()?;
    let home_core = unsafe { percpu::core_id() as u32 };
    setup_process(id, binary, phys_offset, caps, true, home_core).ok()?;
    Some(id)
}

/// Index of the first Empty table slot, if any.
fn find_free_slot() -> Option<usize> {
    // SAFETY: scalar reads of the single-CPU table.
    unsafe { (*addr_of!(TABLE)).iter().position(|s| s.state == State::Empty) }
}

/// Reclaim any processes that were set up before a launch aborted. Only used
/// on the setup-failure path, where nothing has run yet, so each slot still
/// holds its `Process` and `boot_frames`.
fn teardown_all() {
    // SAFETY: the BKL (D4) is held by `run` (its only caller); nothing is
    // running yet.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        for slot in table.iter_mut() {
            if let Some(proc) = slot.process.take() {
                let l4 = proc.l4;
                process::teardown(proc, &slot.boot_frames);
                memory::destroy_address_space(l4);
            }
            *slot = Slot::empty();
        }
        *addr_of_mut!(WAIT_LINKS) = [None; MAX_PROCESSES];
        // The whole table was just wiped above, so every CORE_QUEUE entry
        // pointing into it is stale too -- a full wipe is simpler and exactly
        // as correct as removing each one individually.
        *addr_of_mut!(CORE_QUEUE) = [[None; MAX_PROCESSES]; percpu::MAX_CORES];
    }
}

/// Loop forever, claiming and running any Ready process in this core's own
/// queue, halting between attempts. Entered by every AP once its per-core
/// infrastructure is up (Stage B2.3, `smp.rs::ap_entry64`); the BSP reaches
/// the same claim logic through `switch_to_next`'s existing idle path once
/// `run`'s own initial slot has exited. Unlike `idle_until_runnable` (which
/// requires an existing blocked input/disk waiter to justify waiting, else it
/// would mask a genuine deadlock), this is a true idle task: an AP with
/// nothing claimable yet is not a deadlock, just an idle core, so it waits
/// unconditionally for the next reschedule IPI (`setup_process`/`wake_with`)
/// or device IRQ. Never returns.
pub fn ap_idle_loop() -> ! {
    // SAFETY: percpu::init has already run on this core (smp.rs, before this
    // is reached).
    let me = unsafe { percpu::core_id() as u32 };
    loop {
        bkl::acquire();
        // SAFETY: the BKL is held; `next` is freshly looked up under it.
        if let Some(next) = first_claimable_slot(me) {
            unsafe { resume_process(next) }; // never returns
        }
        // S2: nothing of mine is Ready -- try stealing one Ready process
        // from another core before halting.
        if let Some(stolen) = try_steal(me) {
            unsafe { resume_process(stolen) }; // never returns
        }
        // BKL (D4): release before halting, same discipline as
        // idle_until_runnable -- another core (or this core's own later
        // wake) needs the lock free to make progress while we wait.
        unsafe { bkl::release() };
        x86_64::instructions::interrupts::enable_and_hlt();
        x86_64::instructions::interrupts::disable();
    }
}
