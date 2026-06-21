/* User link layout for musl-linked oxbow programs. Like userland/sbase/user.ld
 * but adds what musl needs: a PT_TLS segment (so the kernel's ELF loader builds a
 * TLS block + sets fs_base from PT_TLS) and .init_array/.fini_array with the
 * __init_array_start/end symbols musl's __libc_start_init iterates. Entry is
 * oxbow-rt's _start (which calls our oxbow_main crt bridge). */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5);   /* RX */
    rodata PT_LOAD FLAGS(4);   /* R  */
    data   PT_LOAD FLAGS(6);   /* RW */
    tls    PT_TLS  FLAGS(4);
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

    /* Thread-local storage: tdata is initialized (lives in the data load segment)
     * AND is described by PT_TLS for the loader's TLS template. */
    .tdata : {
        *(.tdata .tdata.*)
    } :data :tls
    .tbss : {
        *(.tbss .tbss.*)
        *(.tcommon)
    } :data :tls

    /* C runtime init/fini arrays — musl __libc_start_init walks these. */
    .init_array : {
        PROVIDE_HIDDEN(__init_array_start = .);
        KEEP(*(SORT_BY_INIT_PRIORITY(.init_array.*)))
        KEEP(*(.init_array))
        PROVIDE_HIDDEN(__init_array_end = .);
    } :data
    .fini_array : {
        PROVIDE_HIDDEN(__fini_array_start = .);
        KEEP(*(SORT_BY_INIT_PRIORITY(.fini_array.*)))
        KEEP(*(.fini_array))
        PROVIDE_HIDDEN(__fini_array_end = .);
    } :data

    .data : {
        *(.data .data.*)
        *(.data.rel.ro .data.rel.ro.*)
        *(.got .got.plt)
    } :data

    .bss : {
        *(.bss .bss.*)
        *(COMMON)
    } :data

    /DISCARD/ : {
        *(.eh_frame*)
        *(.note .note.*)
        *(.comment)
    }
}
