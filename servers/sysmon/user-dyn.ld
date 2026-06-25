/* §96 Phase 3: dynamic link layout for sysmon (a PT_INTERP'd non-PIE ET_EXEC that
 * imports oxui from /lib/liboxui.so). Modeled on servers/dyntest/dyn.ld but with
 * sysmon's full section set. Keeps ehdr+phdrs in the first PT_LOAD (FILEHDR PHDRS on
 * :text) so the dynamic linker's AT_PHDR (DynInfo.exe_phdr) is mapped. */
OUTPUT_FORMAT(elf64-x86-64)
ENTRY(_start)
PHDRS {
    phdr    PT_PHDR PHDRS;
    interp  PT_INTERP;
    text    PT_LOAD FILEHDR PHDRS; /* ehdr+phdrs in the first LOAD -> AT_PHDR mapped */
    rodata  PT_LOAD;
    data    PT_LOAD;
    dynamic PT_DYNAMIC;
}
SECTIONS {
    . = 0x200000 + SIZEOF_HEADERS;
    .interp   : { *(.interp) }            :text :interp
    .text     : { *(.text .text.*) }      :text
    .plt      : { *(.plt) }               :text
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .rodata   : { *(.rodata .rodata.*) }  :rodata
    .dynsym   : { *(.dynsym) }            :rodata
    .dynstr   : { *(.dynstr) }            :rodata
    .gnu.hash : { *(.gnu.hash) }          :rodata
    .hash     : { *(.hash) }              :rodata
    .rela.dyn : { *(.rela.dyn) }          :rodata
    .rela.plt : { *(.rela.plt) }          :rodata
    . = ALIGN(CONSTANT(MAXPAGESIZE));
    .data     : { *(.data .data.*) }      :data
    .got      : { *(.got) }               :data
    .got.plt  : { *(.got.plt) }           :data
    .dynamic  : { *(.dynamic) }           :data :dynamic
    .bss      : { *(.bss .bss.*) *(COMMON) } :data
    /DISCARD/ : { *(.eh_frame*) *(.note .note.*) *(.comment) }
}
