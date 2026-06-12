/* oxbow kernel link script — higher-half at -2GiB, Limine protocol.
 * The .requests* sections carry the Limine request markers and must be
 * KEEP()-t and discoverable by the bootloader.
 * The __*_start/__*_end symbols give the kernel its own section boundaries so
 * it can map its image W^X-clean in Phase 3 (text RX, rodata R, data RW+NX). */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(kmain)

PHDRS
{
    requests PT_LOAD;
    text     PT_LOAD;
    rodata   PT_LOAD;
    data     PT_LOAD;
}

SECTIONS
{
    /* Top 2 GiB of the 64-bit address space; matches -C code-model=kernel. */
    . = 0xffffffff80000000;
    __kernel_start = .;

    /* .requests and .text both fall in our kernel R-X mapping. They must sit on
     * SEPARATE pages, though: Limine loads .requests as a RW segment and .text
     * as RX, and two PT_LOADs cannot share a page with different perms. */
    __text_start = .;
    .requests : {
        KEEP(*(.requests_start_marker))
        KEEP(*(.requests))
        KEEP(*(.requests_end_marker))
    } :requests

    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .text : {
        *(.text .text.*)
    } :text
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __text_end = .;

    __rodata_start = .;
    .rodata : {
        *(.rodata .rodata.*)
    } :rodata
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __rodata_end = .;

    __data_start = .;
    .data : {
        *(.data .data.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    __data_end = .;

    __kernel_end = .;

    /* Bare metal: drop unwinding tables and notes we don't use. */
    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
