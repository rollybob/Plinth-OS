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
use core::sync::atomic::{AtomicBool, Ordering};

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
    /// The core this process is pinned to (Stage B2.3, D5), once claimed.
    /// `None` while Ready and never yet run -- any core may claim it, which
    /// sets this exactly once; from then on only that core's claim loop ever
    /// picks it again (no cross-core migration). Cleared back to `None` when
    /// the slot is reclaimed at death (`Slot::empty()`), so the next process
    /// that lands in this slot starts unowned again.
    owner: Option<u32>,
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
            owner: None,
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
pub const GP_RCX: usize = 2;
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

/// `states()`, but as seen by core `me` (Stage B2.3, D5): a Ready slot
/// already pinned to a DIFFERENT core is hidden from `pick_next`'s pure
/// round-robin -- presented as `Running` (in use, just not by `me`) rather
/// than a new `State` variant, since `pick_next` only ever branches on
/// `== State::Ready`. An unowned slot, or one already `me`'s own, is left
/// genuinely `Ready`. Keeps `pick_next` itself untouched and still pure/
/// tested against plain `[State; N]` arrays.
fn states_for(me: u32) -> [State; MAX_PROCESSES] {
    let mut s = states();
    // SAFETY: scalar reads of the single-CPU-per-core table; the BKL is held
    // by every caller.
    unsafe {
        let table = &*addr_of!(TABLE);
        for i in 0..MAX_PROCESSES {
            if s[i] == State::Ready && table[i].owner.is_some_and(|o| o != me) {
                s[i] = State::Running;
            }
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
    if !from_user || !quantum_expired() {
        unsafe { bkl::release() };
        return frame as u64;
    }

    let me = unsafe { percpu::core_id() as u32 };
    let cur = unsafe { (*addr_of!(CURRENT_SLOT))[me as usize] };
    // Filtered by ownership (Stage B2.3, D5): never preempt onto a process
    // pinned to a different core.
    let Some(next) = pick_next(&states_for(me), cur) else {
        // Nobody else is runnable (e.g. the synchronous demos, where the
        // table is empty): keep running this process. This is exactly the
        // Stage-1 "count and return" behavior.
        unsafe { bkl::release() };
        return frame as u64;
    };

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
        // D5: claim `next` for this core the first time anyone runs it; once
        // owned, it never moves to a different core.
        if table[next].owner.is_none() {
            table[next].owner = Some(me);
        }

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
/// the slot Ready. Reuses the same loader the synchronous path uses.
fn setup_process(
    id: usize,
    binary: &[u8],
    phys_offset: u64,
    caps: &[Option<Capability>],
    notify: bool,
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
    // New Ready work is unowned (Stage B2.3, D5) until some core's claim
    // loop picks it up, so wake every other online core out of `hlt` to go
    // look -- it may otherwise sleep forever never noticing. The caller
    // (run(), spawn()) holds the BKL across this whole function, same as
    // every other TABLE write. `run()` sets up several processes per call;
    // its caller sends one IPI after the whole batch instead of one per
    // process (notify=false here) to avoid issuing several ICR sends back
    // to back with no gap.
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

    for (id, binary) in binaries.iter().take(count).enumerate() {
        let grant = extra.get(id).copied().flatten();
        if let Err(e) = setup_process(id, binary, phys_offset, &[grant], false) {
            let _ = writeln!(serial, "plinth: {label}: setup of process {id} failed: {e}");
            // Reclaim whatever was set up before aborting the demo.
            teardown_all();
            unsafe { bkl::release() };
            return;
        }
    }
    // One IPI for the whole batch (not one per process, Stage B2.3): slots
    // 1..count are unowned Ready work an idle AP may claim; slot 0 is about
    // to be claimed inline below, so it needs no IPI of its own.
    if count > 1 {
        crate::irq::send_reschedule_ipi();
    }

    SCHEDULER_ACTIVE.store(true, Ordering::Relaxed);

    // Install the first process as CURRENT and enter it. sched_start returns
    // only once every process has exited (via on_exit -> sched_return_to_kernel).
    // SAFETY: slot 0 was just set up Ready; the BKL is held, IF=0 here.
    unsafe {
        let me = percpu::core_id() as u32;
        let table = &mut *addr_of_mut!(TABLE);
        let proc = table[0].process.take().expect("first slot has no process");
        table[0].state = State::Running;
        table[0].owner = Some(me); // D5: this core claims slot 0 outright
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
    let cur = unsafe { (*addr_of!(CURRENT_SLOT))[percpu::core_id()] };

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
    }

    // The current process is gone; switch_to_next decides what "nothing else
    // claimable" means from here (launcher vs. AP, table empty vs. other
    // cores' work still live -- see NoWorkAction's doc).
    unsafe { switch_to_next(NoWorkAction::ExitedReturnIfDone) }
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

/// What "nothing left for THIS core to claim" should do once `switch_to_next`
/// confirms it (no Ready/claimable slot, no input/disk waiter to justify
/// `idle_until_runnable`). Resolved against two further facts at the point of
/// idling, both of which can change between when the caller decided which
/// variant to pass and when this is actually evaluated (Stage B2.3 is
/// genuinely concurrent): whether THIS core is the launcher
/// (`IS_LAUNCHER`), and whether the table is truly empty everywhere
/// (`table_entirely_empty`) -- another core's still-live process might yet
/// wake this one.
enum NoWorkAction {
    /// Reached from `on_exit`. If the table is now empty everywhere, every
    /// process has truly exited: the launcher returns to `run`'s caller; any
    /// other core (an AP) has nothing to return to and just keeps idling --
    /// it may pick up the next demo's work later.
    ExitedReturnIfDone,
    /// Reached from `block_current`. If the table is empty everywhere, this
    /// process's block can never be satisfied: a genuine deadlock. Otherwise
    /// some other core's live process might still wake it via `wake_with` --
    /// not (yet) a deadlock, keep idling.
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
/// # Safety
/// Caller must have already removed the leaving process from `CURRENT` (taken
/// it for teardown, or parked it in its slot as Blocked), must hold the BKL
/// (D4), and must hold no other locks.
unsafe fn switch_to_next(on_idle: NoWorkAction) -> ! {
    let me = percpu::core_id() as u32;
    loop {
        let cur = CURRENT_SLOT[me as usize];
        // Filtered by ownership (D5): never pick up a process pinned elsewhere.
        let states = states_for(me);
        if let Some(next) = pick_next(&states, cur) {
            resume_process(next); // never returns
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
        // skipping `cur`). `on_exit` already emptied `cur`'s slot before
        // calling here, so this is harmless (always false) on that path.
        if states[cur] == State::Ready {
            resume_process(cur); // never returns
        }

        // Nothing claimable BY ME. A process blocked on EXTERNAL input or on
        // disk I/O is not a deadlock -- a keystroke or a virtio completion
        // IRQ can still arrive -- so idle and wait for it rather than treat
        // it as stuck.
        let input_waiter = crate::input::any_waiter();
        if input_waiter || crate::virtio_blk::any_waiter() {
            // Deterministic delivery for headless smoke: if a synthetic
            // keyboard event is armed, deliver it now (it wakes the blocked
            // reader). Real keystrokes -- and every disk completion -- arrive
            // via their device IRQ during the idle below.
            if input_waiter {
                crate::input::deliver_synthetic();
            }
            idle_until_runnable(); // never returns
        }

        if table_entirely_empty() {
            match (on_idle, IS_LAUNCHER[me as usize]) {
                // SAFETY: sched_start saved the anchor before entering the
                // first process, and its stack frame is still live.
                (NoWorkAction::ExitedReturnIfDone, true) => {
                    // BKL (D4): returning to `run`'s caller, ordinary
                    // (non-dispatch) boot-sequence code -- release before
                    // this longjmp, the same as the ring-3 chokepoint in
                    // resume_process.
                    bkl::release();
                    sched_return_to_kernel(0); // never returns
                }
                (NoWorkAction::ExitedReturnIfDone, false) => {
                    // Not the launcher -- no anchor to return to, and the
                    // table really is empty, so there is nothing to claim
                    // either; park exactly like any other idle core.
                    bkl::release();
                    ap_idle_loop(); // never returns
                }
                (NoWorkAction::BlockedDeadlockIfNoOtherWork, _) => {
                    // Panics, halting the system -- no release needed.
                    panic!("scheduler: every process is blocked (IPC deadlock)")
                }
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
    // D5: claim `next` for this core the first time anyone runs it; once
    // owned, it never moves to a different core. (A no-op if `next` is
    // already `me`'s own, e.g. a process this core blocked and is now
    // resuming.)
    if table[next].owner.is_none() {
        table[next].owner = Some(me);
    }
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
        // pick_next skips, so scan for any Ready slot this core may claim
        // (unowned, or already its own -- D5).
        if let Some(next) = first_claimable_slot(me) {
            resume_process(next);
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

/// Index of the first Ready slot core `me` may claim -- unowned, or already
/// its own (Stage B2.3, D5: never a process pinned to a different core).
/// Unlike `pick_next` (round-robin, which skips the current slot), this
/// includes the current slot -- the idle loop must resume a reader that
/// blocked and was just woken in place. Shared by `idle_until_runnable`
/// (must already have a reason to wait -- a blocked input/disk waiter) and
/// `ap_idle_loop` (waits for any claimable work at all).
fn first_claimable_slot(me: u32) -> Option<usize> {
    // SAFETY: scalar reads of the table; the BKL is held by every caller.
    unsafe {
        (*addr_of!(TABLE))
            .iter()
            .position(|s| s.state == State::Ready && s.owner.is_none_or(|o| o == me))
    }
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
    // The slot was already owned (only a Running process can have blocked)
    // (Stage B2.3, D5) -- its owning core may be halted waiting for exactly
    // this; wake every other online core out of `hlt` to go check. Slightly
    // wasteful (only the owner cares) but simple and correct; targeted IPIs
    // are a B3-style optimization, not needed before there is evidence this
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
        let cur = CURRENT_SLOT[percpu::core_id()];
        let running = process::current()
            .lock()
            .take()
            .expect("no CURRENT process to block");
        let table = &mut *addr_of_mut!(TABLE);
        table[cur].kernel_rsp = frame_ptr;
        table[cur].process = Some(running);
        table[cur].state = State::Blocked;
        switch_to_next(NoWorkAction::BlockedDeadlockIfNoOtherWork)
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
    setup_process(id, binary, phys_offset, caps, true).ok()?;
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
    }
}

/// Loop forever, claiming and running any Ready process this core may claim
/// (unowned, or already its own -- D5), halting between attempts. Entered by
/// every AP once its per-core infrastructure is up (Stage B2.3,
/// `smp.rs::ap_entry64`); the BSP reaches the same claim logic through
/// `switch_to_next`'s existing idle path once `run`'s own initial slot has
/// exited. Unlike `idle_until_runnable` (which requires an existing blocked
/// input/disk waiter to justify waiting, else it would mask a genuine
/// deadlock), this is a true idle task: an AP with nothing claimable yet is
/// not a deadlock, just an idle core, so it waits unconditionally for the
/// next reschedule IPI (`setup_process`/`wake_with`) or device IRQ. Never
/// returns.
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
        // BKL (D4): release before halting, same discipline as
        // idle_until_runnable -- another core (or this core's own later
        // wake) needs the lock free to make progress while we wait.
        unsafe { bkl::release() };
        x86_64::instructions::interrupts::enable_and_hlt();
        x86_64::instructions::interrupts::disable();
    }
}
