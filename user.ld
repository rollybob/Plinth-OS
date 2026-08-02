/* The linker script for every user crate.
 *
 * ONE script, shared. Each crate's build.rs points at this file. It used to be
 * 39 per-crate copies, which is one decision written in 39 places -- exactly
 * what I7 forbids -- and on 2026-08-01 that cost what duplication always costs:
 * the .got problem below was diagnosed once, fixed in 3 of the 39, and the
 * version that propagated was the wrong one. See the note at the bottom.
 *
 * Load address must match USER_IMAGE_BASE in the kernel's process.rs. With
 * per-process address spaces every user crate can share the same link base:
 * they live in separate page tables.
 *
 * Each output section group is page-aligned so the linker emits separate,
 * page-aligned PT_LOAD segments (R-X text, R-- rodata, RW- data). The kernel's
 * ELF loader requires page-aligned segments and enforces per-page W^X, so text
 * and rodata must not share a page with writable data.
 *
 * The kernel honors e_entry, so _start may sit anywhere in .text -- no special
 * first-section placement is needed.
 */

ENTRY(_start)
OUTPUT_FORMAT(elf64-x86-64)

SECTIONS {
    . = 0x400000;

    .text : { *(.text .text.*) }

    . = ALIGN(0x1000);
    .rodata : { *(.rodata .rodata.*) }

    . = ALIGN(0x1000);
    .data : {
        *(.data .data.*)

        /* .got MUST be placed explicitly, and it MUST land here rather than in
         * .rodata. Both halves of that were learned the hard way.
         *
         * Left unnamed, .got is an ORPHAN section. The linker parks an orphan
         * straight after the last section with matching flags -- immediately
         * after .rodata, at whatever unaligned address that ended on. Because
         * .got is writable, that orphan becomes a PT_LOAD of its own with an
         * unaligned p_vaddr, and the kernel's loader rejects the binary at spawn
         * with "elf: segment vaddr not page-aligned". The process never starts,
         * and the error names ELF while the cause is here.
         *
         * A crate grows a .got when something takes an address that needs one --
         * a call to compiler_builtins' weak memcpy/memset will do it, and so
         * will a large enough cross-crate call graph. shell-user had none until
         * it gained a libos dependency, then emitted exactly one 8-byte entry.
         * So "this crate does not have one" is a property of today's code, not
         * of the crate, which is why every crate names it.
         *
         * Folding it into .rodata also works, in the sense that the binary
         * boots -- three crates did that and none of them broke. But an input
         * section's flags propagate to its output section, so a writable .got
         * inside .rodata makes the WHOLE rodata segment writable, and the binary
         * silently loses W^X on its constants. rwfs-user shipped that way: 744
         * bytes of .rodata with the write bit set, where every other crate had
         * it clear. Nothing failed, nothing was asserted, and it would not have
         * been noticed. .data is where writable data belongs.
         *
         * .got.plt and .data.rel.ro are named pre-emptively for the same reason:
         * there is no dynamic loader here, so both are ordinary writable data,
         * but each is an orphan waiting to reproduce the failure above the first
         * time some crate emits one. */
        *(.got .got.plt)
        *(.data.rel.ro .data.rel.ro.*)
    }

    /* .bss needs its own ALIGN, and the lack of one was a second latent bug in
     * the per-crate scripts -- found on 2026-08-01 the moment they were merged,
     * which is the argument for having merged them.
     *
     * .bss is NOBITS, so the linker gives it a PT_LOAD of its own rather than
     * extending .data's. Without this ALIGN it simply starts wherever .data
     * ended, and the loader rejects the binary exactly as it does for an orphan
     * .got. It never fired before because it needs BOTH a non-page-multiple
     * .data and a non-empty .bss in the same crate, and no crate had both:
     * every crate with a .bss happened to have an empty .data. Adding .got to
     * .data above gave two of them 8 bytes of .data and broke both immediately
     * (kbd-user, gfxtext-user).
     *
     * The alternative -- folding .bss into the .data output section so they
     * share a segment -- is wrong: .bss would become PROGBITS and its zeros
     * would be written into the file, which is the one thing .bss exists to
     * avoid. A page of padding is the cheaper trade. */
    . = ALIGN(0x1000);
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
