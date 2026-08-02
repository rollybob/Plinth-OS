ENTRY(_start)
OUTPUT_FORMAT(elf64-x86-64)

/* Load address must match USER_IMAGE_BASE in the kernel's process.rs. With
 * per-process address spaces, every user crate can share the same link base:
 * they live in separate page tables.
 *
 * Each output section group is page-aligned so the linker emits separate,
 * page-aligned PT_LOAD segments (R-X text, R-- rodata, RW- data). The kernel's
 * ELF loader requires page-aligned segments and enforces per-page W^X. */

SECTIONS {
    . = 0x400000;

    .text : { *(.text .text.*) }

    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) }

    . = ALIGN(0x1000);
    .data : {
        *(.data .data.*)
        /* .got MUST be placed explicitly. Left unnamed it is an ORPHAN section,
         * and the linker parks an orphan straight after the last section with
         * matching flags -- here immediately after .rodata, at whatever
         * unaligned address that ended on. Because .got is writable, that
         * orphan then becomes a PT_LOAD of its own with an unaligned p_vaddr,
         * and the kernel's ELF loader rejects the binary at spawn with
         * "elf: segment vaddr not page-aligned". The process never starts.
         *
         * This crate grew a .got (8 bytes, one entry) when it gained a libos
         * dependency for the two-source input ring. Nothing about the section is
         * special -- it is writable data, so it belongs in the .data group with
         * the rest, and naming it here is what stops the linker guessing.
         *
         * .got.plt and .data.rel.ro are named for the same reason, pre-emptively:
         * there is no dynamic loader here, so they are ordinary writable data,
         * but each is an orphan waiting to reproduce this failure the first time
         * some crate emits one. */
        *(.got .got.plt)
        *(.data.rel.ro .data.rel.ro.*)
    }

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    }

    /DISCARD/ : {
        *(.comment)
        *(.eh_frame)
        *(.eh_frame_hdr)
    }
}
