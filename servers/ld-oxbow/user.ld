/* §96: ld-oxbow — a static ET_EXEC linked at 0x1000_0000 so it shares the address
 * space with (but never overlaps) the dynamic executable it links (at 0x200000) or
 * the shared objects it maps (bumped from 0x3000_0000). W^X-clean sections. */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    text   PT_LOAD;
    rodata PT_LOAD;
    data   PT_LOAD;
}

SECTIONS
{
    . = 0x10000000;

    .text : {
        *(.text .text.*)
    } :text
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .data : {
        *(.data .data.*)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
    }
}
