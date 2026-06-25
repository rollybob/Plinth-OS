//! AP bring-up -- broader hardware, Stage B1 + B2.2 (Design/broader_hardware.md
//! section 5.1/5.2, section 8).
//!
//! The BSP wakes each Application Processor with the INIT-SIPI-SIPI sequence
//! via the Local APIC, pointing it at a 16-bit real-mode trampoline placed at
//! a fixed low physical address (`TRAMPOLINE_PHYS`, reserved out of the frame
//! allocator in `frame_alloc.rs`, exactly like the frame-0 reservation). The
//! trampoline carries the AP through real -> protected -> long mode using a
//! small throwaway GDT of its own (the kernel's real GDT is built in kernel
//! virtual address space, unreachable before paging is on), loads the SAME
//! `kernel_l4` page table the BSP already uses, and lands in `ap_entry64` --
//! ordinary Rust, with its own one-frame stack.
//!
//! Stage B1 proved an AP could be woken and reach Rust at all -- it marked
//! itself alive and parked, touching nothing the BSP's structures use. Stage
//! B2.2 (this revision) has `ap_entry64` go on to bring up its own per-core
//! infrastructure -- GDT/TSS, IDT, syscall MSRs, GS_BASE-reached per-CPU data
//! (percpu.rs), and its own Local APIC -- through the exact same calls the
//! BSP's boot path uses, parameterized by a dense core id this module hands
//! each AP through the trampoline's param block. It still runs nothing but
//! this one function and parks in `cli; hlt` afterward: nothing it does races
//! with the BSP yet, because it never touches a TABLE/CURRENT/ENDPOINTS-style
//! shared structure -- joining the scheduler is Stage B2.3's job.
//!
//! Two correctness hazards specific to sharing the BSP's existing `kernel_l4`
//! with a freshly-started core, both flagged in Design/broader_hardware.md
//! section 10:
//!
//! - The trampoline's own code, while still executing from its low physical
//!   address, must stay mapped at THAT SAME address the instant paging turns
//!   on -- `kernel_l4`'s normal mappings are all `phys_offset + phys`, which
//!   does not cover raw low addresses at their own value. `memory::map_identity`
//!   adds a transient identity mapping for exactly the trampoline's one page
//!   before any AP is woken; `memory::unmap_identity` removes it once every AP
//!   has moved past it into ordinary, phys-offset-mapped kernel code
//!   (`ap_entry64`).
//! - `EFER.NXE` is a per-core MSR, not shared CPU state. The bootloader's
//!   physical-memory window maps RAM with the NX bit set (sensible: that
//!   window is data, never meant to be executed from) and enables NXE on the
//!   BSP before handing off to `kernel_main` so the bit means what it says.
//!   An AP that loads the same page tables WITHOUT also setting NXE sees that
//!   same NX bit as a *reserved* bit instead -- the CPU raises a reserved-bit
//!   page fault on first touch of anything in that window (caught empirically
//!   2026-06-20: the AP triple-faulted reaching for its own stack, which
//!   lives in exactly that window, until EFER.NXE was added alongside LME).
//!
//! Clean-room: built from the public Intel SDM / MP-startup algorithm and the
//! generic `x86-assembly-boot`/`cpu-topology-osdev` OSdev references, not from
//! any other kernel's SMP/bring-up code.

// The trampoline's `global_asm!` (below) deliberately switches assembler syntax
// for the two AT&T-only far jumps, tripping `bad_asm_style`. The directives are
// the only way to express a two-instruction syntax switch (`options(att_syntax)`
// is whole-block), so allow the lint for this module -- an item-level allow on
// the `global_asm!` itself is not honored for this particular lint.
#![allow(bad_asm_style)]

use core::fmt::Write;
use core::ptr::addr_of;

use crate::frame_alloc::FRAME_SIZE;
use crate::{acpi, irq, memory, percpu, timer};

/// Fixed physical address the AP trampoline lives at, and the SIPI vector
/// names (vector = `TRAMPOLINE_PHYS >> 12`). Chosen, not discovered -- this is
/// the kernel's own structure, the same way a chosen MMIO mapping is. Verified
/// at boot to fall inside a `Usable` region (2026-06-20: confirmed under
/// OVMF/QEMU, `[0x0, 0x87000)` is one contiguous Usable region). Reserved out
/// of the frame allocator (`frame_alloc.rs`) so it is never handed out as an
/// ordinary frame.
pub const TRAMPOLINE_PHYS: u64 = 0x8000;

// Layout within the trampoline page, all literal-constant offsets the
// trampoline assembly below addresses directly (see the module doc on why
// these must be literals, never label-derived addresses). Code lives at
// offset 0 (where the SIPI vector lands); everything else is well clear of
// it, checked at runtime in `install_trampoline`.
const GDT_OFFSET: u64 = 0x100; // 4 descriptors x 8 bytes
const GDT_PTR_OFFSET: u64 = 0x120; // 6 bytes: u16 limit, u32 base
const PARAM_KERNEL_L4_OFFSET: u64 = 0x128; // u32: kernel_l4 phys (low dword)
const PARAM_STACK_TOP_OFFSET: u64 = 0x130; // u64: this AP's stack top (kernel VA)
const PARAM_ENTRY_OFFSET: u64 = 0x138; // u64: ap_entry64's address (kernel VA)
/// This AP's dense core id (Stage B2.2, D6) -- this kernel's own `0..MAX_CORES`
/// index (percpu.rs), distinct from its APIC id. Read by `ap_entry64` through
/// the still-active identity mapping, same as `STATUS_OFFSET` below.
const PARAM_CORE_ID_OFFSET: u64 = 0x140; // u64: this AP's percpu core id
/// Offset where `ap_entry64` (Rust) writes its "I'm alive" marker, addressed
/// through the still-active identity mapping (see the module doc). Far enough
/// past the trampoline code and the param block to never collide with either,
/// checked at runtime.
const STATUS_OFFSET: u64 = 0xFF0;
const STATUS_ALIVE: u32 = 0xCAFE_BABE;

// A minimal GDT for the 16->32->64 transition only -- NOT the kernel's real
// GDT (gdt.rs), which lives in kernel virtual address space and is therefore
// unreachable until paging is on. Selectors: 0x08 = 32-bit flat code, 0x10 =
// 32-bit flat data, 0x18 = 64-bit code (L-bit set, D-bit clear per the Intel
// SDM requirement that a 64-bit code descriptor not also claim the legacy
// 32-bit default-operand-size bit). Bit positions per Intel SDM Vol 3A,
// Figure 3-8; the same construction the `x86-assembly-boot` skill's GDT64
// example uses, extended with a 32-bit code/data pair for the protected-mode
// leg this kernel's UEFI boot path didn't otherwise need.
const GDT_NULL: u64 = 0;
const GDT_CODE32: u64 =
    0xFFFF | (1 << 41) | (1 << 43) | (1 << 44) | (1 << 47) | (0xF << 48) | (1 << 54) | (1 << 55);
const GDT_DATA32: u64 = 0xFFFF | (1 << 41) | (1 << 44) | (1 << 47) | (0xF << 48) | (1 << 54) | (1 << 55);
const GDT_CODE64: u64 = (1 << 41) | (1 << 43) | (1 << 44) | (1 << 47) | (1 << 53);

// The trampoline. Assembled as ordinary bytes inside the kernel's own .text
// (wherever the linker puts it) and copied verbatim to TRAMPOLINE_PHYS at
// runtime -- it never executes from its link location, so every address it
// touches is either a literal physical constant (computed from
// TRAMPOLINE_PHYS by hand: the GDT pointer, the kernel_l4/stack-top param
// slots, the status word) or a position-independent relative branch/near
// label (safe under a verbatim copy) or a GAS link-time CONSTANT EXPRESSION
// (`label - ap_trampoline16_start + TRAMPOLINE_PHYS`, resolved at assemble
// time to the correct runtime address regardless of where the blob is linked
// -- the standard technique for an intra-blob far jump target). The two far
// jumps switch to AT&T syntax for exactly one instruction each: GAS's
// immediate-operand far jump (`ljmp $sel, $off`) is the well-documented form
// for this transition and only exists in AT&T syntax. The FINAL jump, from
// the trampoline's long-mode tail into `ap_entry64`, reads `ap_entry64`'s
// address out of the param block (PARAM_ENTRY_OFFSET) rather than encoding it
// as an asm-time immediate: this kernel links as a PIE (`rust-lld` rejects an
// absolute R_X86_64_64 relocation against a function symbol), so the address
// must come from a relocation the COMPILER resolves correctly (an ordinary
// Rust `fn` pointer cast, written into the param block in
// `install_trampoline`) rather than one hand-assembled here.
//
// The blob is Intel syntax (the default); the two far jumps are the only
// instructions that must drop to AT&T (`.att_syntax prefix`) and back, because
// LLVM's integrated assembler -- which `global_asm!` uses, NOT GAS -- will not
// encode an immediate far jump in Intel syntax. Confirmed against the pinned
// toolchain: every Intel spelling (`jmp 0x08:..`, `ljmp 0x08:..`, `jmp far ..`)
// errors; only the AT&T `ljmp $sel, $off` assembles. `options(att_syntax)` is
// whole-block, so it cannot express a two-instruction switch; the local
// directives are deliberate, and `bad_asm_style` is allowed at module level
// above.
core::arch::global_asm!(
    r#"
.global ap_trampoline16_start
.global ap_trampoline16_end
.code16
ap_trampoline16_start:
    cli
    xor ax, ax
    mov ds, ax
    mov ss, ax

    lgdt [0x8120]

    mov eax, cr0
    or eax, 1
    mov cr0, eax

.att_syntax prefix
    ljmp $0x08, $(protected32 - ap_trampoline16_start + 0x8000)
.intel_syntax noprefix

.code32
protected32:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax

    mov eax, cr4
    or eax, (1 << 5)
    mov cr4, eax

    mov eax, [0x8128]
    mov cr3, eax

    mov ecx, 0xC0000080
    rdmsr
    or eax, (1 << 8) | (1 << 11)
    wrmsr

    mov eax, cr0
    or eax, (1 << 31)
    mov cr0, eax

.att_syntax prefix
    ljmp $0x18, $(long64 - ap_trampoline16_start + 0x8000)
.intel_syntax noprefix

.code64
long64:
    mov rsp, [0x8130]
    mov rax, [0x8138]
    jmp rax
ap_trampoline16_end:
.code64
"#
);

extern "C" {
    static ap_trampoline16_start: u8;
    static ap_trampoline16_end: u8;
}

/// Bound on the trampoline blob's size: it must fit before `GDT_OFFSET` with
/// room to spare. A few dozen bytes today; checked at runtime
/// (`install_trampoline`) rather than assumed.
const MAX_TRAMPOLINE_LEN: u64 = GDT_OFFSET;

/// The AP's landing point in Rust, reached via the trampoline's final `jmp`
/// with paging on, `kernel_l4` loaded (so ordinary phys-offset-mapped kernel
/// code and data are reachable), and RSP already pointing at this AP's own
/// stack (`alloc_ap_stack`). Marks itself alive -- through the trampoline
/// page's still-active identity mapping, so the literal `TRAMPOLINE_PHYS`
/// virtual address resolves to the right physical page without needing
/// `phys_offset` plumbed all the way down here -- then parks forever.
///
/// Stage B2.2 brought up each core's own GDT/TSS, IDT, syscall MSRs, per-CPU
/// (GS_BASE) data, and Local APIC -- the same per-core infrastructure the BSP
/// sets up for itself at boot, built by the identical code paths
/// (gdt::init/percpu::init/syscall::init/interrupts::load_on_this_core/
/// irq::enable_lapic_ap) so there is exactly one way any core comes up, not a
/// BSP path and a separate AP path. Stage B2.3 (this revision) has it go on
/// to join `scheduler::ap_idle_loop`, which claims and runs any Ready process
/// this core may claim (D5) and halts between attempts -- never returns.
extern "C" fn ap_entry64() -> ! {
    // SAFETY: TRAMPOLINE_PHYS is still identity-mapped at this point -- the
    // BSP only calls `memory::unmap_identity` after every AP it started has
    // either reported in here or timed out -- so this virtual address
    // resolves to the trampoline page's own physical frame, the same frame
    // `start_aps` polls via the ordinary phys-offset mapping.
    // SAFETY: read PARAM_CORE_ID_OFFSET into a local *before* announcing
    // STATUS_ALIVE -- `start_aps`'s loop treats "alive" as permission to
    // reuse this same param block for the *next* AP (new core_id, new
    // stack_top, status reset to 0) and only waits in ~1ms polling
    // increments, not for this AP to finish reading it. Writing status
    // first (the previous order) raced two real, concurrently-running
    // cores: with 2+ APs to bring up, "alive" could be observed and the
    // block recycled before this core's own read executed, handing it a
    // stale or already-overwritten core_id (Stage B2.3 -- a real bug found
    // by booting under PLINTH_SMP=3/4, never PLINTH_SMP=2 where there is
    // only one AP and the block is never reused).
    let core_id = unsafe {
        let id = core::ptr::read_volatile((TRAMPOLINE_PHYS + PARAM_CORE_ID_OFFSET) as *const u64)
            as usize;
        core::ptr::write_volatile((TRAMPOLINE_PHYS + STATUS_OFFSET) as *mut u32, STATUS_ALIVE);
        id
    };

    // Ordinary kernel code/data from here on -- `kernel_l4`'s normal mappings
    // cover it, the same as any other kernel function; no identity mapping or
    // phys_offset needed (only the two trampoline-page touches above did).
    let selectors = crate::gdt::init(core_id);
    crate::percpu::init(core_id, crate::syscall::stack_top(core_id));
    crate::syscall::init(&selectors);
    crate::interrupts::load_on_this_core();
    crate::irq::enable_lapic_ap();
    timer::arm_ap();

    // Interrupts stay disabled until the idle loop's own sti;hlt (`cli` ran
    // in the 16-bit trampoline and nothing above re-enabled it) -- the same
    // discipline `idle_until_runnable` already uses, applied to a core that
    // has never run anything yet rather than one between scheduled processes.
    crate::scheduler::ap_idle_loop()
}

/// Write the trampoline's fixed, AP-independent setup: the blob itself, the
/// temporary GDT and its pointer, and the `kernel_l4` param slot. Also installs
/// the transient identity mapping the mode transition needs (module doc).
/// Call once before waking any AP.
fn install_trampoline(phys_offset: u64) -> Result<(), &'static str> {
    // SAFETY: `ap_trampoline16_start`/`_end` bound the assembled blob (linker
    // symbols from the global_asm! above); reading their extent and copying it
    // is the standard trampoline-staging pattern. The destination is the
    // reserved, never-otherwise-mapped trampoline page, addressed the normal
    // phys-offset way (the BSP itself, unlike the AP mid-transition, already
    // has that mapping).
    unsafe {
        let start = addr_of!(ap_trampoline16_start);
        let end = addr_of!(ap_trampoline16_end);
        let len = end.offset_from(start) as u64;
        assert!(len < MAX_TRAMPOLINE_LEN, "AP trampoline blob grew past its reserved layout");
        let dst = (phys_offset + TRAMPOLINE_PHYS) as *mut u8;
        core::ptr::copy_nonoverlapping(start, dst, len as usize);

        let gdt = [GDT_NULL, GDT_CODE32, GDT_DATA32, GDT_CODE64];
        for (i, entry) in gdt.iter().enumerate() {
            let addr = phys_offset + TRAMPOLINE_PHYS + GDT_OFFSET + (i as u64) * 8;
            core::ptr::write_volatile(addr as *mut u64, *entry);
        }
        // The 6-byte GDTR image (u16 limit, u32 base) is packed for `lgdt` to
        // read as-is; that packs the u32 base at a 2-byte (not 4-byte)
        // boundary, so it needs an unaligned write, not a volatile one (this
        // is one-time setup before any AP can read it -- no concurrent
        // access to race, unlike the polled status word below).
        let gdt_ptr_base = phys_offset + TRAMPOLINE_PHYS + GDT_PTR_OFFSET;
        core::ptr::write_unaligned(gdt_ptr_base as *mut u16, (gdt.len() * 8 - 1) as u16);
        core::ptr::write_unaligned(
            (gdt_ptr_base + 2) as *mut u32,
            (TRAMPOLINE_PHYS + GDT_OFFSET) as u32,
        );

        let kernel_l4 = memory::kernel_l4();
        assert!(kernel_l4 <= u32::MAX as u64, "kernel_l4 phys address needs a 32-bit CR3 load");
        core::ptr::write_volatile(
            (phys_offset + TRAMPOLINE_PHYS + PARAM_KERNEL_L4_OFFSET) as *mut u32,
            kernel_l4 as u32,
        );

        // ap_entry64 as a function pointer, cast to an integer: the compiler
        // resolves this correctly under PIE (unlike a hand-assembled `movabs`
        // immediate, see the trampoline's doc comment above).
        core::ptr::write_volatile(
            (phys_offset + TRAMPOLINE_PHYS + PARAM_ENTRY_OFFSET) as *mut u64,
            ap_entry64 as *const () as u64,
        );

        core::ptr::write_volatile((phys_offset + TRAMPOLINE_PHYS + STATUS_OFFSET) as *mut u32, 0);
    }
    memory::map_identity(TRAMPOLINE_PHYS, FRAME_SIZE)
}

/// Each AP's own boot stack: not a process kernel stack (those exist only
/// once a process is set up, scheduler.rs's KSTACKS) and not its syscall
/// stack (syscall.rs's SYSCALL_STACKS, armed only once `syscall::init` runs)
/// -- this is what's live from the moment `ap_entry64` starts until that
/// core claims its first process, AND what `ap_idle_loop` keeps running on
/// in between claims (it never switches its own rsp). A single 4 KiB frame
/// (Stage B1's size, when a core did nothing but write one word and `hlt`)
/// is nowhere near enough once that loop is doing real work under
/// contention: acquiring/releasing the BKL, calling `resume_process`,
/// fielding a reschedule IPI mid-halt -- with 2+ APs all doing this at once
/// (Stage B2.3) it overflowed into whatever frame happened to follow it,
/// corrupting that memory and crashing in a way that looked like a jump
/// through a corrupted pointer (an instruction-fetch #PF on a data page) --
/// a real bug found by booting under PLINTH_SMP=3/4 (never PLINTH_SMP=2,
/// the only configuration with just one AP and so the least contention).
/// Sized to match `scheduler::KSTACK_SIZE`; static like every other per-core
/// stack in this kernel (gdt.rs, syscall.rs, scheduler.rs) rather than
/// frame-allocated, so it is an ordinary mapped kernel VA from the start --
/// no phys-offset indirection needed.
const AP_STACK_SIZE: usize = 16 * 4096;
#[repr(align(16))]
struct ApStack(#[allow(dead_code)] [u8; AP_STACK_SIZE]);
static mut AP_BOOT_STACKS: [ApStack; percpu::MAX_CORES] =
    [const { ApStack([0; AP_STACK_SIZE]) }; percpu::MAX_CORES];

/// This AP's boot stack top (a kernel VA `ap_entry64` will run with).
fn alloc_ap_stack(core_id: usize) -> u64 {
    // SAFETY: address arithmetic over the static; no reference taken. Each
    // core_id is assigned to exactly one AP (start_aps), so no two cores
    // ever use the same slot.
    unsafe { core::ptr::addr_of!(AP_BOOT_STACKS[core_id]) as u64 + AP_STACK_SIZE as u64 }
}

/// Read the status word an AP's `ap_entry64` writes once it has run.
fn read_status(phys_offset: u64) -> u32 {
    // SAFETY: a plain volatile read of the reserved trampoline page's status
    // word; the AP writes it once with an ordinary (non-atomic, but
    // single-writer) store -- single producer (this one AP), single consumer
    // (this poll), never running concurrently (one AP brought up at a time),
    // the same bounded single-writer/single-reader discipline `virtio_blk`'s
    // completion poll uses.
    unsafe { core::ptr::read_volatile((phys_offset + TRAMPOLINE_PHYS + STATUS_OFFSET) as *const u32) }
}

// Local APIC Interrupt Command Register (ICR) offsets (irq::ICR_LOW/
// ICR_HIGH) are shared with irq.rs's own reschedule-IPI sender.
/// Delivery mode INIT (Intel SDM Table 10-1), in ICR_LOW bits 8..11.
const DELIVERY_INIT: u32 = 0b101 << 8;
/// Delivery mode Start-Up (the SIPI), in ICR_LOW bits 8..11.
const DELIVERY_STARTUP: u32 = 0b110 << 8;
/// Level (assert), ICR_LOW bit 14. The startup algorithm asserts INIT; this
/// implementation skips the explicit legacy de-assert pulse some very old
/// chipsets needed (Intel's MP spec calls it optional on any APIC that
/// supports the simplified startup algorithm, which every CPU QEMU emulates
/// does).
const LEVEL_ASSERT: u32 = 1 << 14;

/// Send the INIT-SIPI-SIPI sequence to `target_apic_id`, pointing the AP at
/// the trampoline. Intel's universal startup algorithm (MP spec, also Intel
/// SDM Vol 3A 10.6.5): INIT, wait ~10ms, SIPI, wait ~200us, SIPI again (the
/// second SIPI is redundant on hardware that took the first, but required by
/// spec since some chipsets need it).
fn send_init_sipi(lapic_va: u64, target_apic_id: u8) {
    let vector = (TRAMPOLINE_PHYS >> 12) as u32;
    let dest = (target_apic_id as u32) << 24;
    // SAFETY: lapic_va is the mapped LAPIC (the caller only reaches this under
    // irq::apic_mode()); these are the defined ICR registers, programmed
    // sequentially with bounded waits between, never concurrently (one CPU
    // bringing up one AP at a time).
    unsafe {
        irq::lapic_reg_write(lapic_va, irq::ICR_HIGH, dest);
        irq::lapic_reg_write(lapic_va, irq::ICR_LOW, DELIVERY_INIT | LEVEL_ASSERT);
        timer::busy_wait_us(10_000);

        for _ in 0..2 {
            irq::lapic_reg_write(lapic_va, irq::ICR_HIGH, dest);
            irq::lapic_reg_write(lapic_va, irq::ICR_LOW, DELIVERY_STARTUP | vector);
            timer::busy_wait_us(200);
        }
    }
}

/// Bound on how long `start_aps` waits for one AP to report alive, in 1ms
/// `busy_wait_us` steps -- a wedged/absent AP fails this one core's bring-up
/// instead of hanging boot, the same no-silent-hang discipline as everywhere
/// else (`keyboard.rs`'s `POLL_MAX`, `virtio_blk.rs`'s completion poll).
const AP_WAIT_POLLS: u32 = 200; // ~200ms per AP

/// Wake every CPU the MADT reported other than the BSP, carrying each one
/// through real -> protected -> long mode into `ap_entry64`, where it parks.
/// Call once at boot, after `irq::init` (needs the LAPIC up to send IPIs). A
/// `None` topology (no MADT) or a missing BSP id means there is no LAPIC to
/// send IPIs from, so this is a no-op -- the same "PIC fallback has no APIC
/// infrastructure at all" reasoning Stage A2/A3 already apply.
pub fn start_aps<W: Write>(out: &mut W, topology: Option<&acpi::Topology>, phys_offset: u64) {
    let (Some(topo), Some(lapic_va), Some(bsp_id)) =
        (topology, irq::lapic_base(), irq::bsp_apic_id())
    else {
        let _ = writeln!(out, "plinth: smp: no LAPIC (no MADT), skipping AP bring-up");
        return;
    };

    if let Err(e) = install_trampoline(phys_offset) {
        let _ = writeln!(out, "plinth: smp: trampoline install failed: {e}");
        return;
    }

    let mut online = 0usize;
    // Dense core ids (percpu.rs), distinct from APIC ids: the BSP is
    // percpu::BSP_CORE_ID (0); each AP brought up here gets the next one.
    // Assigned regardless of whether this particular AP ends up online --
    // simplest, and a timed-out AP's reserved id is simply never used.
    let mut next_core_id = percpu::BSP_CORE_ID + 1;
    for &apic_id in &topo.cpu_apic_ids[..topo.cpu_id_count] {
        if apic_id == bsp_id {
            continue; // the BSP is already running this code
        }
        let core_id = next_core_id;
        next_core_id += 1;
        if core_id >= percpu::MAX_CORES {
            let _ = writeln!(out, "plinth:   smp: apic id {apic_id} skipped (MAX_CORES reached)");
            continue;
        }

        let stack_top = alloc_ap_stack(core_id);
        // SAFETY: writing this AP's stack top, core id, and clearing the
        // status word before waking it; only this AP's trampoline run
        // touches any of them before the next iteration's writes -- one AP
        // brought up at a time.
        unsafe {
            core::ptr::write_volatile(
                (phys_offset + TRAMPOLINE_PHYS + PARAM_STACK_TOP_OFFSET) as *mut u64,
                stack_top,
            );
            core::ptr::write_volatile(
                (phys_offset + TRAMPOLINE_PHYS + PARAM_CORE_ID_OFFSET) as *mut u64,
                core_id as u64,
            );
            core::ptr::write_volatile(
                (phys_offset + TRAMPOLINE_PHYS + STATUS_OFFSET) as *mut u32,
                0,
            );
        }
        send_init_sipi(lapic_va, apic_id);

        let mut polls = 0u32;
        while read_status(phys_offset) != STATUS_ALIVE && polls < AP_WAIT_POLLS {
            timer::busy_wait_us(1_000);
            polls += 1;
        }

        if read_status(phys_offset) == STATUS_ALIVE {
            online += 1;
            irq::mark_ap_online(core_id, apic_id);
            let _ = writeln!(out, "plinth:   smp: apic id {apic_id} online");
        } else {
            let _ = writeln!(out, "plinth:   smp: apic id {apic_id} did not respond");
        }
    }

    // The identity mapping was only ever needed for the few instructions
    // between "enable paging" and "jmp into ordinary kernel code" -- every AP
    // that made it to ap_entry64 is long past needing it, and one that timed
    // out is not going to use it later either. Tear it down per
    // Design/broader_hardware.md section 10.
    memory::unmap_identity(TRAMPOLINE_PHYS, FRAME_SIZE);

    let _ = writeln!(out, "plinth: smp: {online} ap(s) online");
}
