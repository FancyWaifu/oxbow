/* User link layout for oxbow programs: a static ET_EXEC in the low half.
 * Each section gets its own page so the kernel's ELF loader can map them
 * W^X-clean (text RX, rodata R, data RW+NX). Entry is oxbow-rt's _start.
 * §101: .tdata/.tbss form a PT_TLS template the kernel copies per-thread. */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    text   PT_LOAD;
    rodata PT_LOAD;
    data   PT_LOAD;
    tls    PT_TLS;
}

SECTIONS
{
    . = 0x200000;

    .text : {
        *(.text .text.*)
    } :text
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    /* Thread-local template: .tdata (initialized, file-backed) + .tbss (NOBITS).
     * Mapped into both the data PT_LOAD (so .tdata's bytes are loaded) and the
     * PT_TLS segment (the per-thread template the kernel copies). */
    .tdata : {
        *(.tdata .tdata.*)
    } :data :tls
    .tbss : {
        *(.tbss .tbss.*)
        *(.tcommon)
    } :data :tls

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
