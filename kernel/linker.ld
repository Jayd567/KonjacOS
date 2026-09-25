/* Linker script for KonjacOS, targeting the Limine boot protocol (x86_64). */

OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_entry)

PHDRS
{
    text    PT_LOAD    FLAGS((1 << 0) | (1 << 2)) ; /* Execute + Read */
    rodata  PT_LOAD    FLAGS((1 << 2)) ;             /* Read only */
    data    PT_LOAD    FLAGS((1 << 1) | (1 << 2)) ;  /* Write + Read */
}

SECTIONS
{
    /* Load the kernel at the higher half, as expected by Limine's default
       (non-relocatable) placement for the x86_64 Limine protocol. */
    . = 0xffffffff80000000;

    .text : {
        *(.text .text.*)
    } :text

    . += CONSTANT(MAXPAGESIZE);

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata

    /* Limine request structs must live in their own section so the
       start/end markers in main.rs bound exactly the request pointers and
       nothing else. */
    .requests : {
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
    } :rodata

    . += CONSTANT(MAXPAGESIZE);

    .data : {
        *(.data .data.*)
    } :data

    /* Even with a fully static, non-PIC link, LLD emits GOT-indirect calls
       for a handful of core::fmt entry points. Those GOT slots are still
       resolved to fixed addresses at link time (no runtime relocation is
       needed), so we just need to keep the section instead of discarding
       it -- dropping it left those `jmp *offset(%rip)` sites reading
       unmapped near-null memory (this was a real bug caught by booting in
       QEMU: immediate page fault -> double fault -> triple fault reboot).
       .plt is unused for a static binary but harmless to keep too. */
    .got : {
        *(.got .got.*)
    } :data

    .plt : {
        *(.plt .plt.*)
    } :data

    .bss : {
        *(COMMON)
        *(.bss .bss.*)
    } :data

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
        *(.rela.*)
    }
}
