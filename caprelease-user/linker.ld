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
    /* .got folded in defensively, as rwfs-user's script does: a call to
     * compiler_builtins' weak memcpy/memset goes through a GOT entry, and an
     * unmatched .got gets orphaned into its own unaligned segment, which the
     * ELF loader rejects. This crate's buffers are small enough that it
     * probably emits none -- an empty match costs nothing, and the failure it
     * prevents is a confusing loader rejection rather than a link error. */
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
