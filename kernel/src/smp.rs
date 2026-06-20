//! AP bring-up -- broader hardware, Stage B1 (Design/broader_hardware.md
//! section 5.1, section 8).
//!
//! The BSP wakes each Application Processor with the INIT-SIPI-SIPI sequence
//! via the Local APIC, pointing it at a 16-bit real-mode trampoline placed at
//! a fixed low physical address (`TRAMPOLINE_PHYS`, reserved out of the frame
//! allocator in `frame_alloc.rs`, exactly like the frame-0 reservation). The
//! trampoline carries the AP through real -> protected -> long mode using a
//! small throwaway GDT of its own (the kernel's real GDT is built in kernel
//! virtual address space, unreachable before paging is on), loads the SAME
//! `kernel_l4` page table the BSP already uses, and lands in `ap_entry64` --
//! ordinary Rust, with its own one-frame stack -- which marks itself alive and
//! parks in `cli; hlt` forever.
//!
//! This milestone is deliberately narrow: prove an AP can be woken and reach
//! Rust at all. It does not yet touch the kernel's shared structures'
//! concurrency (Stage B2) -- the AP never takes an interrupt, never loads an
//! IDT, and never runs anything but this one function, so nothing it does
//! races with the BSP.
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

use core::fmt::Write;
use core::ptr::addr_of;

use crate::frame_alloc::{FRAME_ALLOC, FRAME_SIZE};
use crate::{acpi, irq, memory, timer};

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
/// Stage B1 scope: this core never loads its own GDT/IDT/TSS (the temporary
/// trampoline GDT's flat descriptors are perfectly valid for a core that
/// never takes a ring transition or an interrupt) and never runs anything
/// else. `cli` from the 16-bit trampoline is still in effect and is never
/// lifted, so no exception can be delivered here even if the bytes above this
/// core somehow faulted; per-CPU GDT/IDT/TSS plumbing is Stage B2's job, once
/// a core actually has work to do.
extern "C" fn ap_entry64() -> ! {
    // SAFETY: TRAMPOLINE_PHYS is still identity-mapped at this point -- the
    // BSP only calls `memory::unmap_identity` after every AP it started has
    // either reported in here or timed out -- so this virtual address
    // resolves to the trampoline page's own physical frame, the same frame
    // `start_aps` polls via the ordinary phys-offset mapping.
    unsafe {
        core::ptr::write_volatile((TRAMPOLINE_PHYS + STATUS_OFFSET) as *mut u32, STATUS_ALIVE);
    }
    loop {
        // SAFETY: parks forever with interrupts already disabled (`cli` ran
        // in the 16-bit trampoline and is never lifted) -- see the doc above
        // on why no IDT is needed for that to be safe in Stage B1.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
    }
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

/// Allocate one frame as an AP's kernel stack and return its top (the
/// phys-offset virtual address `ap_entry64` will run with). One frame is
/// generous for Stage B1: the core does nothing but write one word and loop
/// on `hlt`. Stage B2, when a core actually runs scheduled work, sizes this
/// properly (and per-CPU, not per-bring-up).
fn alloc_ap_stack(phys_offset: u64) -> Result<u64, &'static str> {
    let phys = {
        let mut guard = FRAME_ALLOC.lock();
        let fa = guard.as_mut().ok_or("frame allocator not initialised")?;
        fa.alloc().map_err(|_| "out of frames for AP stack")?
    };
    Ok(phys_offset + phys + FRAME_SIZE)
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

// Local APIC Interrupt Command Register (ICR): two 32-bit halves. ICR_HIGH's
// bits 24..32 are the physical destination APIC id; writing ICR_LOW issues the
// IPI (Intel SDM Vol 3A, 10.6).
const ICR_LOW: u32 = 0x300;
const ICR_HIGH: u32 = 0x310;
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
        irq::lapic_reg_write(lapic_va, ICR_HIGH, dest);
        irq::lapic_reg_write(lapic_va, ICR_LOW, DELIVERY_INIT | LEVEL_ASSERT);
        timer::busy_wait_us(10_000);

        for _ in 0..2 {
            irq::lapic_reg_write(lapic_va, ICR_HIGH, dest);
            irq::lapic_reg_write(lapic_va, ICR_LOW, DELIVERY_STARTUP | vector);
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
    for &apic_id in &topo.cpu_apic_ids[..topo.cpu_id_count] {
        if apic_id == bsp_id {
            continue; // the BSP is already running this code
        }

        let stack_top = match alloc_ap_stack(phys_offset) {
            Ok(top) => top,
            Err(e) => {
                let _ = writeln!(out, "plinth:   smp: apic id {apic_id} stack alloc failed: {e}");
                continue;
            }
        };
        // SAFETY: writing this AP's stack top and clearing the status word
        // before waking it; only this AP's trampoline run touches either
        // before the next iteration's writes -- one AP brought up at a time.
        unsafe {
            core::ptr::write_volatile(
                (phys_offset + TRAMPOLINE_PHYS + PARAM_STACK_TOP_OFFSET) as *mut u64,
                stack_top,
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
