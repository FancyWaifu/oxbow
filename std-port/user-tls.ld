/* User link layout for oxbow programs: a static ET_EXEC in the low half.
 * Each section gets its own page so the kernel's ELF loader can map them
 * W^X-clean (text RX, rodata R, data RW+NX). Entry is oxbow-rt's _start.
 * §101: .tdata/.tbss form a PT_TLS template the kernel copies per-thread.
 * §102 panic=unwind: keep .eh_frame + .gcc_except_table and define the symbols
 * the `unwinding` crate's static FDE finder needs (__executable_start/__etext/
 * __eh_frame). Harmless for panic=abort programs (empty/unused). */
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
    __executable_start = .;

    .text : {
        *(.text .text.*)
    } :text
    __etext = .;
    . = ALIGN(CONSTANT(MAXPAGESIZE));

    .rodata : {
        *(.rodata .rodata.*)
    } :rodata
    /* DWARF unwind tables: KEEP so --gc-sections can't drop them. __eh_frame marks
     * the start for the static FDE finder. .gcc_except_table is the LSDA the Rust
     * personality routine reads to find cleanup/catch landing pads. */
    . = ALIGN(8);
    __eh_frame = .;
    .eh_frame : {
        KEEP(*(.eh_frame))
        *(.eh_frame.*)
    } :rodata
    .gcc_except_table : {
        KEEP(*(.gcc_except_table))
        *(.gcc_except_table.*)
    } :rodata
    . = ALIGN(CONSTANT(MAXPAGESIZE));

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
        *(.note .note.*)
    }
}
