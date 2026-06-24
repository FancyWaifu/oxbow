//! §96 ld-oxbow — oxbow's userland dynamic linker (the PT_INTERP interpreter).
//!
//! The kernel loads a dynamically-linked executable AND this interpreter into one
//! address space, sets the thread entry to the interpreter, and maps a [`DynInfo`]
//! page so we can find the executable. We then:
//!   1. read the executable's `PT_DYNAMIC`,
//!   2. load each `DT_NEEDED` shared object from `/lib` (via the slot-1 fs cap):
//!      `sys_map` RW → copy segments → apply `R_X86_64_RELATIVE` → `sys_protect`
//!      text to RX (the W^X-safe RW→RX transition — no kernel relaxation),
//!   3. resolve every `R_X86_64_JUMP_SLOT`/`GLOB_DAT`/`64` against the global symbol
//!      table (executable first, then the libraries — exe-exported libc satisfies a
//!      library's imports; a library's exports satisfy the executable),
//!   4. jump to the executable's real entry.
//!
//! Eager binding only (DT_BIND_NOW), no lazy PLT, no dynamic TLS (libraries must be
//! PT_TLS-free for now), no dlopen. Fixed/bump-allocated load bases.
#![no_std]
#![no_main]

use oxbow_abi::{
    DynInfo, Handle, MsgBuf, DYN_INFO, DYN_INFO_MAGIC, FS_DIR, FS_FILE, HANDLE_NULL, PROT_EXEC,
    PROT_READ, PROT_WRITE, TAG_FS_OPEN, TAG_FS_READ_BULK,
};
use oxbow_rt as rt;

// --- ELF + dynamic constants ------------------------------------------------
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PF_X: u32 = 1;

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_PLTRELSZ: i64 = 2;
const DT_HASH: i64 = 4;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_JMPREL: i64 = 23;

const R_X86_64_64: u32 = 1;
const R_X86_64_GLOB_DAT: u32 = 6;
const R_X86_64_JUMP_SLOT: u32 = 7;
const R_X86_64_RELATIVE: u32 = 8;

const PAGE: u64 = 4096;
const SCRATCH: u64 = 0x5000_0000; // where we read a .so file before mapping its segments
// Sized to fit the default 256 KiB program budget (a small .so + its mapping). Phase
// 3's liboxui.so will need a larger spawn budget; bump both then.
const SCRATCH_LEN: u64 = 128 * 1024;
const SO_BASE_START: u64 = 0x3000_0000; // shared objects bump from here

const MAX_OBJS: usize = 8;

#[repr(C)]
#[derive(Clone, Copy)]
struct Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Dyn {
    d_tag: i64,
    d_val: u64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Sym {
    st_name: u32,
    st_info: u8,
    st_other: u8,
    st_shndx: u16,
    st_value: u64,
    st_size: u64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Rela {
    r_offset: u64,
    r_info: u64,
    r_addend: i64,
}

/// A loaded object (the executable or a shared library) and the dynamic tables we
/// resolve against. All absolute addresses (base already added).
#[derive(Clone, Copy)]
struct Obj {
    base: u64,    // load bias (0 for the non-PIE exe)
    strtab: u64,  // .dynstr
    symtab: u64,  // .dynsym
    nsym: usize,  // symbol count (from DT_HASH nchain)
    rela: u64,    // .rela.dyn
    relasz: u64,  // bytes
    jmprel: u64,  // .rela.plt
    pltrelsz: u64, // bytes
}
const ZERO_OBJ: Obj = Obj { base: 0, strtab: 0, symtab: 0, nsym: 0, rela: 0, relasz: 0, jmprel: 0, pltrelsz: 0 };

unsafe fn rd<T: Copy>(addr: u64) -> T {
    core::ptr::read_unaligned(addr as *const T)
}
unsafe fn wr(addr: u64, v: u64) {
    core::ptr::write_unaligned(addr as *mut u64, v);
}

/// NUL-terminated C-string equality.
unsafe fn cstr_eq(a: u64, b: u64) -> bool {
    let mut i = 0u64;
    loop {
        let ca: u8 = rd(a + i);
        let cb: u8 = rd(b + i);
        if ca != cb {
            return false;
        }
        if ca == 0 {
            return true;
        }
        i += 1;
    }
}

fn die(msg: &str) -> ! {
    rt::println!("[ld] FATAL: {}", msg);
    rt::sys_exit(127)
}

/// Parse an object's `_DYNAMIC` (at `dyn_addr`, pointers biased by `base`) into an
/// `Obj`. Pointer-type DT entries are link-time vaddrs for a `.so`; add the base.
unsafe fn parse_dynamic(base: u64, dyn_addr: u64) -> Obj {
    let mut o = ZERO_OBJ;
    o.base = base;
    let mut hash = 0u64;
    let mut p = dyn_addr;
    loop {
        let d: Dyn = rd(p);
        match d.d_tag {
            DT_NULL => break,
            DT_STRTAB => o.strtab = base + d.d_val,
            DT_SYMTAB => o.symtab = base + d.d_val,
            DT_HASH => hash = base + d.d_val,
            DT_RELA => o.rela = base + d.d_val,
            DT_RELASZ => o.relasz = d.d_val,
            DT_JMPREL => o.jmprel = base + d.d_val,
            DT_PLTRELSZ => o.pltrelsz = d.d_val,
            _ => {}
        }
        p += core::mem::size_of::<Dyn>() as u64;
    }
    // DT_HASH layout: [nbucket, nchain, ...]; nchain == symbol count.
    if hash != 0 {
        o.nsym = rd::<u32>(hash + 4) as usize;
    }
    o
}

/// Resolve a symbol NAME (a C-string at `name_ptr` in `from`'s strtab) to an
/// address, searching every object's defined (`st_shndx != 0`) dynsym entries.
unsafe fn resolve(name_ptr: u64, objs: &[Obj]) -> Option<u64> {
    for o in objs {
        if o.symtab == 0 {
            continue;
        }
        for i in 1..o.nsym {
            let s: Sym = rd(o.symtab + (i * core::mem::size_of::<Sym>()) as u64);
            if s.st_shndx == 0 {
                continue; // UNDEF here
            }
            if cstr_eq(name_ptr, o.strtab + s.st_name as u64) {
                return Some(o.base + s.st_value);
            }
        }
    }
    None
}

/// Apply one RELA table (`addr`..`addr+size`) for object `o`. `relative_only`
/// applies just R_X86_64_RELATIVE (done at .so map time, before the symbol table
/// is complete); otherwise applies the symbol relocations.
unsafe fn apply_rela(o: &Obj, addr: u64, size: u64, objs: &[Obj], relative_only: bool) {
    let n = size / core::mem::size_of::<Rela>() as u64;
    for i in 0..n {
        let r: Rela = rd(addr + i * core::mem::size_of::<Rela>() as u64);
        let ty = (r.r_info & 0xffff_ffff) as u32;
        let sym = (r.r_info >> 32) as usize;
        let where_ = o.base + r.r_offset;
        match ty {
            R_X86_64_RELATIVE if relative_only => wr(where_, o.base.wrapping_add(r.r_addend as u64)),
            R_X86_64_JUMP_SLOT | R_X86_64_GLOB_DAT | R_X86_64_64 if !relative_only => {
                let s: Sym = rd(o.symtab + (sym * core::mem::size_of::<Sym>()) as u64);
                match resolve(o.strtab + s.st_name as u64, objs) {
                    Some(a) => wr(where_, a.wrapping_add(r.r_addend as u64)),
                    None => {
                        rt::println!("[ld] unresolved symbol (obj base {:#x}, reloc {})", o.base, i);
                        die("unresolved symbol");
                    }
                }
            }
            _ => {}
        }
    }
}

// --- fs: read /lib/<name> into the scratch region ---------------------------
fn pack_name(m: &mut MsgBuf, name: &[u8]) {
    let n = core::cmp::min(name.len(), 56);
    let dst = m.data.as_mut_ptr() as *mut u8;
    unsafe {
        core::ptr::copy_nonoverlapping(name.as_ptr(), dst, n);
        *dst.add(n) = 0;
    }
    m.data_len = ((n + 1 + 7) / 8) as u32;
}
/// Read the whole file `cap` into the scratch region; return the byte length.
unsafe fn read_into_scratch(cap: Handle) -> usize {
    let mut off = 0usize;
    loop {
        let want = core::cmp::min(SCRATCH_LEN as usize - off, 4096);
        if want == 0 {
            break;
        }
        let mut m = MsgBuf::new(TAG_FS_READ_BULK);
        m.data[0] = off as u64;
        m.data[1] = want as u64;
        m.data[2] = SCRATCH + off as u64;
        m.data_len = 3;
        if rt::sys_call(cap, &mut m).is_err() {
            break;
        }
        let count = m.data[0] as usize;
        if count == 0 {
            break;
        }
        off += count;
    }
    off
}

/// Load `/lib/<name>` (open in `libdir`) into `*so_base`, map+copy its segments,
/// apply its RELATIVE relocations, flip text to RX, and return its `Obj`.
unsafe fn load_so(libdir: Handle, name: &[u8], so_base: &mut u64, objs: &[Obj]) -> Obj {
    // open + read the file into scratch
    let mut m = MsgBuf::new(TAG_FS_OPEN);
    pack_name(&mut m, name);
    if rt::sys_call(libdir, &mut m).is_err() || m.data[0] != 0 || m.data[1] != FS_FILE {
        die("cannot open shared object");
    }
    let file = m.handles[0];
    let len = read_into_scratch(file);
    let _ = rt::sys_close(file);
    if len < core::mem::size_of::<Ehdr>() {
        die("shared object truncated");
    }
    let eh: Ehdr = rd(SCRATCH);
    // span of the load segments → one RW mapping at the chosen base
    let base = *so_base;
    let mut lo = u64::MAX;
    let mut hi = 0u64;
    for i in 0..eh.e_phnum as u64 {
        let ph: Phdr = rd(SCRATCH + eh.e_phoff + i * eh.e_phentsize as u64);
        if ph.p_type == PT_LOAD {
            lo = lo.min(ph.p_vaddr & !(PAGE - 1));
            hi = hi.max((ph.p_vaddr + ph.p_memsz + PAGE - 1) & !(PAGE - 1));
        }
    }
    if lo == u64::MAX {
        die("shared object has no PT_LOAD");
    }
    let span = hi - lo;
    if rt::sys_map(oxbow_abi::BOOT_MEM, base + lo, span, PROT_READ | PROT_WRITE).is_err() {
        die("sys_map for shared object failed");
    }
    // copy each segment + zero its bss tail
    let mut dyn_addr = 0u64;
    for i in 0..eh.e_phnum as u64 {
        let ph: Phdr = rd(SCRATCH + eh.e_phoff + i * eh.e_phentsize as u64);
        if ph.p_type == PT_DYNAMIC {
            dyn_addr = base + ph.p_vaddr;
        }
        if ph.p_type != PT_LOAD {
            continue;
        }
        core::ptr::copy_nonoverlapping(
            (SCRATCH + ph.p_offset) as *const u8,
            (base + ph.p_vaddr) as *mut u8,
            ph.p_filesz as usize,
        );
        if ph.p_memsz > ph.p_filesz {
            core::ptr::write_bytes(
                (base + ph.p_vaddr + ph.p_filesz) as *mut u8,
                0,
                (ph.p_memsz - ph.p_filesz) as usize,
            );
        }
    }
    if dyn_addr == 0 {
        die("shared object has no PT_DYNAMIC");
    }
    let mut o = parse_dynamic(base, dyn_addr);
    // RELATIVE relocations first (self-relative; need no symbols).
    apply_rela(&o, o.rela, o.relasz, objs, true);
    apply_rela(&o, o.jmprel, o.pltrelsz, objs, true);
    // flip executable segments to RX (W^X-safe RW→RX).
    for i in 0..eh.e_phnum as u64 {
        let ph: Phdr = rd(SCRATCH + eh.e_phoff + i * eh.e_phentsize as u64);
        if ph.p_type == PT_LOAD && ph.p_flags & PF_X != 0 {
            let v0 = (base + ph.p_vaddr) & !(PAGE - 1);
            let v1 = (base + ph.p_vaddr + ph.p_memsz + PAGE - 1) & !(PAGE - 1);
            let _ = rt::sys_protect(oxbow_abi::BOOT_MEM, v0, v1 - v0, PROT_READ | PROT_EXEC);
        }
    }
    *so_base = base + span + PAGE; // bump (+guard) for the next .so
    o.base = base;
    o
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    let info: DynInfo = unsafe { rd(DYN_INFO) };
    if info.magic != DYN_INFO_MAGIC {
        die("not a dynamic spawn (no DynInfo)");
    }
    // scratch region for reading .so files
    if rt::sys_map(oxbow_abi::BOOT_MEM, SCRATCH, SCRATCH_LEN, PROT_READ | PROT_WRITE).is_err() {
        die("sys_map scratch failed");
    }
    // open /lib via the slot-1 fs cap (the executable's fs capability, shared in-process)
    let libdir: Handle = unsafe {
        let mut m = MsgBuf::new(TAG_FS_OPEN);
        pack_name(&mut m, b"lib");
        if rt::sys_call(1 as Handle, &mut m).is_ok() && m.data[0] == 0 && m.data[1] == FS_DIR {
            m.handles[0]
        } else {
            HANDLE_NULL
        }
    };

    let mut objs = [ZERO_OBJ; MAX_OBJS];
    let mut nobjs = 0usize;

    // The executable: find PT_DYNAMIC from its phdrs (mapped via DynInfo).
    let exe_dyn = unsafe {
        let mut d = 0u64;
        for i in 0..info.exe_phnum {
            let ph: Phdr = rd(info.exe_phdr + i * info.exe_phent);
            if ph.p_type == PT_DYNAMIC {
                d = info.exe_base + ph.p_vaddr;
            }
        }
        d
    };
    if exe_dyn == 0 {
        die("executable has no PT_DYNAMIC");
    }
    objs[0] = unsafe { parse_dynamic(info.exe_base, exe_dyn) };
    nobjs = 1;

    // Load each DT_NEEDED shared object.
    let mut so_base = SO_BASE_START;
    unsafe {
        let mut p = exe_dyn;
        loop {
            let d: Dyn = rd(p);
            if d.d_tag == DT_NULL {
                break;
            }
            if d.d_tag == DT_NEEDED {
                if libdir == HANDLE_NULL {
                    die("no /lib capability for DT_NEEDED");
                }
                if nobjs >= MAX_OBJS {
                    die("too many shared objects");
                }
                let name = objs[0].strtab + d.d_val; // C-string in the exe's .dynstr
                // copy the name into a small stack buffer for pack_name
                let mut nb = [0u8; 64];
                let mut k = 0usize;
                while k < nb.len() - 1 {
                    let c: u8 = rd(name + k as u64);
                    if c == 0 {
                        break;
                    }
                    nb[k] = c;
                    k += 1;
                }
                let lib = load_so(libdir, &nb[..k], &mut so_base, &objs[..nobjs]);
                objs[nobjs] = lib;
                nobjs += 1;
            }
            p += core::mem::size_of::<Dyn>() as u64;
        }
    }

    // Resolve symbol relocations now that every object's dynsym is registered.
    unsafe {
        for i in 0..nobjs {
            let o = objs[i];
            apply_rela(&o, o.rela, o.relasz, &objs[..nobjs], false);
            apply_rela(&o, o.jmprel, o.pltrelsz, &objs[..nobjs], false);
        }
    }

    // Hand off to the executable's real entry.
    unsafe {
        let entry = info.exe_entry;
        core::arch::asm!("jmp {0}", in(reg) entry, options(noreturn));
    }
}
