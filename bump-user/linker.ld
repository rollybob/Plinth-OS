ENTRY(_start)
OUTPUT_FORMAT(elf64-x86-64)

/* Load address must match USER_CODE_VA in the kernel's process.rs. */

SECTIONS {
    . = 0x400000;

    /* Entry point must be the very first bytes of the image. */
    .text : {
        KEEP(*(.text.entry))
        *(.text .text.*)
    }

    .rodata : { *(.rodata .rodata.*) }

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
