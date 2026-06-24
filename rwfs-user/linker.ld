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
    /* .got: compiler_builtins' memcpy/memset are weak symbols, so calls into
     * them (emitted for any bulk copy/zero too large to inline -- this
     * crate's in-memory metadata buffer is the first user binary big enough
     * to trigger one) go through a GOT entry rather than a direct call. With
     * zero dynamic relocations in this statically-linked ET_EXEC (confirmed:
     * the GOT's two entries are link-time-resolved constants, never written
     * at runtime), it is read-only data, so it belongs in this section --
     * left unmatched here, the linker orphans it into its own unaligned
     * segment, which the kernel's ELF loader rejects. */
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
