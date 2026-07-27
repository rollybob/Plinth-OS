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
    /* .got folded in defensively -- see rwfs-user's script for why an orphaned
     * .got section is rejected by the ELF loader. This crate is two calls long
     * and will not emit one; the empty match costs nothing. */
    .rodata : { *(.rodata .rodata.*) *(.got) }

    . = ALIGN(0x1000);
    .data : { *(.data .data.*) }

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
