//! Per-CPU data reached through `IA32_GS_BASE` (Stage B2, design D6 --
//! Design/broader_hardware.md section 5.2).
//!
//! Only the scalars a naked-asm stub actually needs `gs:`-relative live
//! here: `syscall_entry`'s stack-switch scratch and `sched_start`/
//! `sched_return_to_kernel`'s anchor (scheduler.rs) -- both single
//! `[rip + STATIC]` globals today, which must become per-core so two cores
//! running them concurrently don't clobber each other. Everything else that
//! is logically per-core but only ever touched from plain Rust (the
//! scheduler's `CURRENT_SLOT`/`TICKS_IN_QUANTUM`, `process::current()`, the
//! per-core TSS/GDT) is an ordinary `[T; MAX_CORES]` array indexed by
//! `core_id()` -- no reason to cram those into the raw GS struct too.
//!
//! No `swapgs`: nothing in this kernel ever lets ring 3 touch the GS
//! *selector* (`gdt::init` loads CS/SS/DS/ES only) or `GS_BASE`, so the
//! kernel can point `GS_BASE` at its own per-core struct once (at boot for
//! the BSP, at bring-up for each AP) and never touch it again. `swapgs`
//! exists to handle ring 3 having changed GS itself, a hazard this kernel
//! does not have -- so D6's *structure* (per-CPU data via a CPU-local base)
//! is kept while the swapgs discipline it usually requires is dropped as
//! unneeded complexity. Flagged in the B2 plan for Timothy's awareness.

use x86_64::registers::model_specific::GsBase;
use x86_64::VirtAddr;

/// Upper bound on online cores. Generous for a toy kernel's `-smp` test
/// range (2/3/4, Design/broader_hardware.md Stage B1); raise if a demo ever
/// needs more.
pub const MAX_CORES: usize = 8;

/// The boot processor's core id, by convention -- the same role `apic_id 0`
/// usually plays, but kept distinct: a core id is this kernel's own dense
/// index (`0..MAX_CORES`), not an APIC id (which can be sparse/non-zero for
/// the BSP on real hardware, though never under QEMU/OVMF).
pub const BSP_CORE_ID: usize = 0;

/// The GS-reached struct. `#[repr(C)]` so field order (and therefore the
/// byte offsets `syscall_entry`'s asm hardcodes) is fixed; the `offset_of!`
/// asserts below fail the build if a field is ever reordered or resized
/// instead of silently miscompiling the stack switch.
#[repr(C)]
pub struct PerCpu {
    pub core_id: u64,
    pub syscall_stack_top: u64,
    pub user_rsp_save: u64,
    /// `scheduler.rs`'s `sched_start`/`sched_return_to_kernel` anchor --
    /// the kernel rsp to longjmp back to when this core's last process
    /// exits. One per core so two cores driving their own claim loop don't
    /// share an anchor.
    pub sched_anchor: u64,
}

const fn empty_percpu() -> PerCpu {
    PerCpu { core_id: 0, syscall_stack_top: 0, user_rsp_save: 0, sched_anchor: 0 }
}

/// One slot per possible core, reserved statically (no heap, like the rest
/// of Plinth). `init` points GS_BASE at this core's own slot; never moved or
/// reallocated afterward, so the pointer GS_BASE holds stays valid forever.
static mut PERCPU: [PerCpu; MAX_CORES] = [const { empty_percpu() }; MAX_CORES];

// The *_OFFSET consts below must match these.
const _: () = assert!(core::mem::offset_of!(PerCpu, syscall_stack_top) == 8);
const _: () = assert!(core::mem::offset_of!(PerCpu, user_rsp_save) == 16);
const _: () = assert!(core::mem::offset_of!(PerCpu, sched_anchor) == 24);

/// `gs:`-relative offset of `syscall_stack_top`, for `syscall.rs`'s asm.
pub const SYSCALL_STACK_PTR_OFFSET: i32 = 8;
/// `gs:`-relative offset of `user_rsp_save`, for `syscall.rs`'s asm.
pub const USER_RSP_SAVE_OFFSET: i32 = 16;
/// `gs:`-relative offset of `sched_anchor`, for `scheduler.rs`'s asm.
pub const SCHED_ANCHOR_OFFSET: i32 = 24;

/// Point this core's `GS_BASE` at its own reserved slot and record its
/// syscall-stack top. Call exactly once per core: the BSP during boot, each
/// AP during bring-up (Stage B2.2). `core_id` must be `< MAX_CORES` and must
/// not already be in use by another live core.
pub fn init(core_id: usize, syscall_stack_top: u64) {
    // SAFETY: each core calls this exactly once, for its own distinct
    // `core_id`, before anything on that core reads PERCPU[core_id] -- no two
    // cores ever write the same slot, and a slot is never written again
    // after the writing core starts relying on it.
    unsafe {
        let slot = &mut (*core::ptr::addr_of_mut!(PERCPU))[core_id];
        slot.core_id = core_id as u64;
        slot.syscall_stack_top = syscall_stack_top;
        GsBase::write(VirtAddr::new(slot as *mut PerCpu as u64));
    }
}

/// This core's id, read back through its own `GS_BASE`.
///
/// # Safety
/// `percpu::init` must have already run on this core.
pub unsafe fn core_id() -> usize {
    (*(GsBase::read().as_u64() as *const PerCpu)).core_id as usize
}
