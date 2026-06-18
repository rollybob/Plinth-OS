ENTRY(_start)
OUTPUT_FORMAT(elf64-x86-64)

/* Load address must match USER_IMAGE_BASE in the kernel's process.rs. Each
 * output section group is page-aligned so the linker emits separate,
 * page-aligned PT_LOAD segments (R-X text, R-- rodata, RW- data): the kernel's
 * ELF loader requires page-aligned segments and enforces per-page W^X. */

SECTIONS {
    . = 0x400000;

    .text : { *(.text .text.*) }

    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) }

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
