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
//! - `process::CURRENT` keeps its meaning -- the process on the CPU right now
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
use core::sync::atomic::{AtomicBool, Ordering};

use x86_64::structures::idt::InterruptDescriptorTable;
use x86_64::VirtAddr;

use crate::capability::Capability;
use crate::gdt;
use crate::memory;
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
    /// On the CPU right now; its `Process` is in `process::CURRENT` and its
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
    /// Intrusive link to the next waiter in an endpoint's queue (ipc.rs).
    /// Meaningful only while Blocked and enqueued; the queue nodes are the
    /// process slots themselves, so the wait queue needs no heap.
    wait_next: Option<usize>,
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
            wait_next: None,
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

/// Index in TABLE of the process currently on the CPU.
static mut CURRENT_SLOT: usize = 0;

/// Ticks accumulated toward the current quantum.
static mut TICKS_IN_QUANTUM: u64 = 0;

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

/// Kernel rsp captured by `sched_start`, restored by `sched_return_to_kernel`
/// when the last process exits. Single CPU, no nesting.
#[no_mangle]
static mut SCHED_ANCHOR: u64 = 0;

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
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    mov [rip + SCHED_ANCHOR], rsp
    jmp sched_resume    // rdi already holds the first kernel rsp

.global sched_return_to_kernel
sched_return_to_kernel:
    // rdi = value sched_start returns. Restore the anchor saved above and
    // return to sched_start's caller as if it returned normally. Mirrors
    // kernel_resume.
    mov rsp, [rip + SCHED_ANCHOR]
    mov rax, rdi
    pop r15
    pop r14
    pop r13
    pop r12
    pop rbp
    pop rbx
    ret
"#
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

/// Is the preemptive scheduler currently driving execution?
pub fn active() -> bool {
    SCHEDULER_ACTIVE.load(Ordering::Relaxed)
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

/// Snapshot the slot states for `pick_next` (which is kept pure for testing).
fn states() -> [State; MAX_PROCESSES] {
    let mut s = [State::Empty; MAX_PROCESSES];
    // SAFETY: scalar reads of the single-CPU table.
    unsafe {
        let table = &*addr_of!(TABLE);
        for (i, slot) in table.iter().enumerate() {
            s[i] = slot.state;
        }
    }
    s
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
    // SAFETY: scalar RMW of a single-CPU static, reached only from the timer
    // handler (IF=0).
    unsafe {
        TICKS_IN_QUANTUM += 1;
        if TICKS_IN_QUANTUM >= QUANTUM {
            TICKS_IN_QUANTUM = 0;
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
    timer::note_tick();
    timer::eoi();

    // SAFETY: the stub passes a pointer to the TrapFrame it built on the
    // current kernel stack; valid for this call.
    let from_user = unsafe { (*frame).cs & 3 == 3 };

    // Non-preemptible kernel: only reschedule out of ring 3, and only when the
    // quantum is up. (Counting the quantum regardless of CPL is fine; kernel
    // ticks are rare and never switch.)
    if !from_user || !quantum_expired() {
        return frame as u64;
    }

    let cur = unsafe { CURRENT_SLOT };
    let Some(next) = pick_next(&states(), cur) else {
        // Nobody else is runnable (e.g. the synchronous demos, where the
        // table is empty): keep running this process. This is exactly the
        // Stage-1 "count and return" behavior.
        return frame as u64;
    };

    // Suspend the running process into its slot, with its saved context.
    // SAFETY: single CPU, IF=0; CURRENT holds the running process and no lock
    // is held across the return (the stub does the iretq after we return).
    unsafe {
        let running = process::CURRENT
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
        *process::CURRENT.lock() = Some(nproc);
        CURRENT_SLOT = next;
        memory::switch_to(next_l4);
        gdt::set_kernel_stack(kstack_top(next));
        next_rsp
    }
}

/// Build a process from `binary` into slot `id`: a private address space, the
/// loaded image, a fresh `Process`, and a fabricated initial trap frame on the
/// slot's kernel stack (with rdi = id, so `_start` receives its id). Leaves
/// the slot Ready. Reuses the same loader the synchronous path uses.
fn setup_process(
    id: usize,
    binary: &[u8],
    phys_offset: u64,
    caps: &[Option<Capability>],
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

    for (id, binary) in binaries.iter().take(count).enumerate() {
        let grant = extra.get(id).copied().flatten();
        if let Err(e) = setup_process(id, binary, phys_offset, &[grant]) {
            let _ = writeln!(serial, "plinth: {label}: setup of process {id} failed: {e}");
            // Reclaim whatever was set up before aborting the demo.
            teardown_all();
            return;
        }
    }

    SCHEDULER_ACTIVE.store(true, Ordering::Relaxed);

    // Install the first process as CURRENT and enter it. sched_start returns
    // only once every process has exited (via on_exit -> sched_return_to_kernel).
    // SAFETY: slot 0 was just set up Ready; single CPU, IF=0 here.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        let proc = table[0].process.take().expect("first slot has no process");
        table[0].state = State::Running;
        let rsp = table[0].kernel_rsp;
        let l4 = proc.l4;
        *process::CURRENT.lock() = Some(proc);
        CURRENT_SLOT = 0;
        memory::switch_to(l4);
        gdt::set_kernel_stack(kstack_top(0));
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
    let cur = unsafe { CURRENT_SLOT };

    // Take the dying process out of CURRENT and reclaim it. Move to the kernel
    // address space first so destroy_address_space tears down tables that are
    // no longer the active CR3.
    let proc = process::CURRENT
        .lock()
        .take()
        .expect("no CURRENT process at exit");
    memory::switch_to_kernel();
    let boot_frames = unsafe { (*addr_of!(TABLE))[cur].boot_frames };
    let l4 = proc.l4;
    process::teardown(proc, &boot_frames);
    memory::destroy_address_space(l4);
    // SAFETY: single CPU, IF=0; the slot is the dying process's own.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        table[cur] = Slot::empty();
    }

    // The current process is gone, so when nothing else is runnable it means
    // every process has exited -> return to the launcher.
    unsafe { switch_to_next(LauncherOnIdle::Return) }
}

/// What to do when no process is runnable after the current one leaves the CPU.
enum LauncherOnIdle {
    /// Every process has exited -> return from `run` (used by `on_exit`).
    Return,
    /// The current process blocked but is still live -> nobody can run, which
    /// for a CPU-bound IPC system means a genuine deadlock (used by
    /// `block_current`).
    Deadlock,
}

/// Pick the next Ready process and resume it, or handle the no-runnable case
/// per `on_idle`. Shared by `on_exit` and `block_current`; never returns.
///
/// # Safety
/// Caller must have already removed the leaving process from `CURRENT` (taken
/// it for teardown, or parked it in its slot as Blocked) and must hold no
/// locks.
unsafe fn switch_to_next(on_idle: LauncherOnIdle) -> ! {
    let cur = CURRENT_SLOT;
    match pick_next(&states(), cur) {
        Some(next) => {
            let table = &mut *addr_of_mut!(TABLE);
            let nproc = table[next]
                .process
                .take()
                .expect("Ready slot has no process");
            table[next].state = State::Running;
            let rsp = table[next].kernel_rsp;
            let l4 = nproc.l4;
            *process::CURRENT.lock() = Some(nproc);
            CURRENT_SLOT = next;
            memory::switch_to(l4);
            gdt::set_kernel_stack(kstack_top(next));
            sched_resume(rsp)
        }
        None => match on_idle {
            // SAFETY: sched_start saved the anchor before entering the first
            // process, and its stack frame is still live.
            LauncherOnIdle::Return => sched_return_to_kernel(0),
            LauncherOnIdle::Deadlock => {
                panic!("scheduler: every process is blocked (IPC deadlock)")
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Blocking support for IPC (ipc.rs). The endpoint objects and the matching
// policy live in ipc.rs; the scheduler owns the process state transitions and
// the intrusive wait-queue links (which live in the slots).
// ---------------------------------------------------------------------------

/// Index of the process currently on the CPU.
pub fn current_slot() -> usize {
    // SAFETY: scalar read of a single-CPU static.
    unsafe { CURRENT_SLOT }
}

/// Set / read a slot's intrusive wait-queue link.
pub fn set_wait_next(slot: usize, next: Option<usize>) {
    // SAFETY: single CPU, IF=0; reached only from IPC dispatch.
    unsafe { (*addr_of_mut!(TABLE))[slot].wait_next = next }
}

pub fn wait_next(slot: usize) -> Option<usize> {
    // SAFETY: as above.
    unsafe { (*addr_of!(TABLE))[slot].wait_next }
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

/// Wake a Blocked process: make it Ready and write `rax`/`rdx` into its saved
/// trap frame, so when the scheduler resumes it the blocking IPC call returns
/// those values (rax = the word, rdx = the transferred cap's landing slot or
/// NO_CAP). The receiver reads both; a woken sender ignores rdx (its send
/// wrapper treats rdx as clobbered).
pub fn wake_with(slot: usize, rax: u64, rdx: u64) {
    // SAFETY: the slot is Blocked, so its kernel_rsp points at a valid saved
    // trap frame on its own (live) kernel stack; single CPU, IF=0.
    unsafe {
        let table = &mut *addr_of_mut!(TABLE);
        let frame = table[slot].kernel_rsp as *mut TrapFrame;
        (*frame).gp[GP_RAX] = rax;
        (*frame).gp[GP_RDX] = rdx;
        table[slot].state = State::Ready;
    }
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
    // SAFETY: reached from the IPC interrupt handler with IF=0 and no locks
    // held; CURRENT holds the running process.
    unsafe {
        let cur = CURRENT_SLOT;
        let running = process::CURRENT
            .lock()
            .take()
            .expect("no CURRENT process to block");
        let table = &mut *addr_of_mut!(TABLE);
        table[cur].kernel_rsp = frame_ptr;
        table[cur].process = Some(running);
        table[cur].state = State::Blocked;
        switch_to_next(LauncherOnIdle::Deadlock)
    }
}

/// Launch `binary` as a new scheduled process while the scheduler is running,
/// minting `caps` into it. Returns its slot, or None if the process table is
/// full or setup failed. The new process is Ready and joins the round-robin;
/// this does not switch to it. Used by `sys_spawn` to make spawn a scheduler
/// operation (the child runs alongside its parent) rather than synchronous
/// nesting.
pub fn spawn(binary: &[u8], phys_offset: u64, caps: &[Option<Capability>]) -> Option<usize> {
    let id = find_free_slot()?;
    setup_process(id, binary, phys_offset, caps).ok()?;
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
    // SAFETY: single CPU, IF=0; nothing is running.
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
    }
}
