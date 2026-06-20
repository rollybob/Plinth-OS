//! ACPI MADT discovery -- Stage A1 of broader hardware (SMP + real devices).
//!
//! The 8259 PIC needed no discovery (fixed ports). The APIC does: the Local
//! APIC base, each I/O APIC's base and global-system-interrupt (GSI) base, the
//! CPU/AP APIC IDs, and the ISA-IRQ -> GSI Interrupt Source Overrides. All of
//! that lives in the ACPI **MADT** (signature "APIC"), reached from the RSDP the
//! bootloader hands us in `BootInfo.rsdp_addr`: RSDP -> RSDT (rev 0/1) or XSDT
//! (rev >= 2) -> the MADT.
//!
//! This module is **pure discovery**: it reads firmware tables through the
//! phys-offset window and reports what it finds. It changes no behaviour -- the
//! PIC still drives interrupts (see `irq`). Bringing up the LAPIC + I/O APIC on
//! top of this map is Stage A2.
//!
//! The model is `pci.rs`: a minimal, hand-rolled, bounded parser that extracts
//! only what Plinth needs, not a general ACPI interpreter (no AML -- the MADT is
//! static table data). Every walk is bounded against a malformed table, and the
//! only assertion the smoke test makes is the stable count summary; the
//! addresses ride unasserted detail lines (they can shift across QEMU versions,
//! exactly like the PCI BARs).
//!
//! Clean-room: built from the public ACPI table layout and the generic OSdev
//! references, not from any other kernel's ACPI code.

use core::fmt::Write;

/// A raw read pointer to physical address `phys`, via the bootloader's
/// physical-memory window (`phys_offset + phys`).
///
/// # Safety
/// `phys` must name physical memory the bootloader mapped (all RAM is), and the
/// caller must only read, at offsets it has bounds-checked against a table
/// length.
unsafe fn ptr_at(phys_offset: u64, phys: u64) -> *const u8 {
    (phys_offset + phys) as *const u8
}

// Unaligned reads: ACPI table fields are packed and not naturally aligned, so
// every multi-byte field goes through `read_unaligned`.
unsafe fn rd_u8(p: *const u8, off: usize) -> u8 {
    core::ptr::read_unaligned(p.add(off))
}
unsafe fn rd_u16(p: *const u8, off: usize) -> u16 {
    core::ptr::read_unaligned(p.add(off) as *const u16)
}
unsafe fn rd_u32(p: *const u8, off: usize) -> u32 {
    core::ptr::read_unaligned(p.add(off) as *const u32)
}
unsafe fn rd_u64(p: *const u8, off: usize) -> u64 {
    core::ptr::read_unaligned(p.add(off) as *const u64)
}

/// Read a 4-byte table signature.
unsafe fn sig4(p: *const u8) -> [u8; 4] {
    [rd_u8(p, 0), rd_u8(p, 1), rd_u8(p, 2), rd_u8(p, 3)]
}

/// The largest number of system description tables we will walk in an RSDT/XSDT,
/// a sanity bound against a corrupt length field (real firmware lists a handful).
const MAX_TABLES: usize = 256;
/// The largest number of MADT entries we will walk, likewise bounded.
const MAX_MADT_ENTRIES: usize = 1024;

/// Discover the CPU + interrupt-controller topology from ACPI and report it.
///
/// Pure discovery: reads only, bounded walks, no behaviour change. `rsdp` is
/// `BootInfo.rsdp_addr` (the RSDP physical address; `None` if the bootloader did
/// not report one). Call once at boot, before the interrupt controller is
/// brought up.
pub fn init<W: Write>(out: &mut W, rsdp: Option<u64>, phys_offset: u64) {
    let Some(rsdp_phys) = rsdp else {
        let _ = writeln!(out, "plinth: acpi: no RSDP reported (skipping discovery)");
        return;
    };

    // SAFETY: rsdp_phys is the firmware RSDP physical address from BootInfo,
    // mapped at phys_offset. We only read; the RSDP is a fixed-size structure
    // and every table walk below is length-bounded.
    unsafe {
        let rsdp_p = ptr_at(phys_offset, rsdp_phys);
        let mut sig = [0u8; 8];
        for (i, b) in sig.iter_mut().enumerate() {
            *b = rd_u8(rsdp_p, i);
        }
        if &sig != b"RSD PTR " {
            let _ = writeln!(out, "plinth: acpi: bad RSDP signature (skipping discovery)");
            return;
        }

        // Revision >= 2 means an ACPI 2.0+ RSDP carrying a 64-bit XSDT; older
        // RSDPs only have the 32-bit RSDT. QEMU q35 provides the XSDT.
        let revision = rd_u8(rsdp_p, 15);
        let madt = if revision >= 2 {
            find_madt(phys_offset, rd_u64(rsdp_p, 24), 8)
        } else {
            find_madt(phys_offset, rd_u32(rsdp_p, 16) as u64, 4)
        };

        match madt {
            Some(madt_phys) => parse_madt(out, phys_offset, madt_phys),
            None => {
                let _ = writeln!(out, "plinth: acpi: MADT not found");
            }
        }
    }
}

/// Walk an RSDT (4-byte entries) or XSDT (8-byte entries) and return the
/// physical address of the MADT (signature "APIC"), if present. The entry count
/// comes from the table's own length and is capped at `MAX_TABLES`.
///
/// # Safety
/// `sdt_phys` must name a system description table in mapped physical memory.
unsafe fn find_madt(phys_offset: u64, sdt_phys: u64, entry_size: usize) -> Option<u64> {
    let p = ptr_at(phys_offset, sdt_phys);
    let length = rd_u32(p, 4) as usize;
    if length < 36 {
        return None; // shorter than an SDT header -> malformed
    }
    let count = ((length - 36) / entry_size).min(MAX_TABLES);
    for i in 0..count {
        let off = 36 + i * entry_size;
        let entry_phys = if entry_size == 8 {
            rd_u64(p, off)
        } else {
            rd_u32(p, off) as u64
        };
        // SAFETY: entry_phys is a firmware-listed table pointer into mapped RAM;
        // we read only its 4-byte signature.
        if &sig4(ptr_at(phys_offset, entry_phys)) == b"APIC" {
            return Some(entry_phys);
        }
    }
    None
}

/// Parse the MADT: log the Local APIC base, each I/O APIC, each enabled CPU's
/// APIC id, and each Interrupt Source Override, then emit the asserted count
/// summary. The entry walk is bounded by the table length and `MAX_MADT_ENTRIES`
/// and stops at the first entry whose length is degenerate or overruns the table.
///
/// # Safety
/// `madt_phys` must name the MADT in mapped physical memory.
unsafe fn parse_madt<W: Write>(out: &mut W, phys_offset: u64, madt_phys: u64) {
    let p = ptr_at(phys_offset, madt_phys);
    let length = rd_u32(p, 4) as usize;
    // The 32-bit Local APIC base, possibly overridden by a type-5 entry below.
    let mut lapic_base = rd_u32(p, 36) as u64;

    let mut cpus = 0usize;
    let mut ioapics = 0usize;
    let mut isos = 0usize;

    // Entries start at offset 44 (after the 36-byte SDT header + the 8 bytes of
    // Local APIC address and flags).
    let mut off = 44usize;
    let mut walked = 0usize;
    while off + 2 <= length && walked < MAX_MADT_ENTRIES {
        walked += 1;
        let etype = rd_u8(p, off);
        let elen = rd_u8(p, off + 1) as usize;
        if elen < 2 || off + elen > length {
            break; // degenerate or overrunning entry -> stop (malformed table)
        }
        match etype {
            0 => {
                // Processor Local APIC: enabled iff flags bit 0.
                let apic_id = rd_u8(p, off + 3);
                if rd_u32(p, off + 4) & 1 != 0 {
                    cpus += 1;
                    let _ = writeln!(out, "plinth:   acpi cpu: apic id {apic_id}");
                }
            }
            1 => {
                // I/O APIC.
                let id = rd_u8(p, off + 2);
                let addr = rd_u32(p, off + 4);
                let gsi_base = rd_u32(p, off + 8);
                ioapics += 1;
                let _ = writeln!(
                    out,
                    "plinth:   acpi ioapic: id {id} base 0x{addr:x} gsi_base {gsi_base}"
                );
            }
            2 => {
                // Interrupt Source Override (ISA IRQ -> GSI remap).
                let source = rd_u8(p, off + 3);
                let gsi = rd_u32(p, off + 4);
                let flags = rd_u16(p, off + 8);
                isos += 1;
                let _ = writeln!(
                    out,
                    "plinth:   acpi iso: irq {source} -> gsi {gsi} flags 0x{flags:x}"
                );
            }
            5 => {
                // Local APIC Address Override: a 64-bit base superseding the
                // 32-bit field in the header.
                lapic_base = rd_u64(p, off + 4);
            }
            9 => {
                // Processor Local x2APIC (used past 255 CPUs): enabled iff bit 0.
                let x2id = rd_u32(p, off + 4);
                if rd_u32(p, off + 8) & 1 != 0 {
                    cpus += 1;
                    let _ = writeln!(out, "plinth:   acpi cpu: x2apic id {x2id}");
                }
            }
            _ => {} // other entry kinds are not needed for Stage A1.
        }
        off += elen;
    }

    let _ = writeln!(out, "plinth:   acpi lapic base 0x{lapic_base:x}");
    let _ = writeln!(out, "plinth:   acpi source overrides: {isos}");
    // The one asserted summary line. Counts are stable under the -smp 1 q35
    // smoke configuration (1 CPU, 1 I/O APIC); the addresses above are not
    // asserted, the way the PCI BAR lines are not.
    let _ = writeln!(out, "plinth: acpi: {cpus} cpu(s), {ioapics} ioapic(s)");
}
