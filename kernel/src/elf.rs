//! Minimal ELF64 support for the v0 server module.
//!
//! Parsing only: validate the header and expose the PT_LOAD segments. The
//! actual mapping/loading lives in `proc`. Module bytes may be unaligned, so all
//! reads go through `read_unaligned`.

const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_TLS: u32 = 7;
const PF_X: u32 = 1;
const PF_W: u32 = 2;
const PF_R: u32 = 4;

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)] // mirrors the on-disk ELF header; not all fields are read
struct Elf64Ehdr {
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
#[allow(dead_code)] // mirrors the on-disk program header; p_paddr/p_align unused
pub struct Elf64Phdr {
    pub p_type: u32,
    pub p_flags: u32,
    pub p_offset: u64,
    pub p_vaddr: u64,
    pub p_paddr: u64,
    pub p_filesz: u64,
    pub p_memsz: u64,
    pub p_align: u64,
}

fn read<T: Copy>(bytes: &[u8], off: usize) -> T {
    assert!(
        off + core::mem::size_of::<T>() <= bytes.len(),
        "elf: read past end of module"
    );
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().add(off) as *const T) }
}

/// A validated view of the module: its entry point and program headers.
pub struct Image<'a> {
    pub entry: u64,
    bytes: &'a [u8],
    phoff: usize,
    phentsize: usize,
    phnum: usize,
}

impl<'a> Image<'a> {
    /// Validate the ELF header per the v0 contract (ABI §7 / D6): 64-bit LE
    /// ET_EXEC for x86_64. Boot-panics on anything else.
    pub fn validate(bytes: &'a [u8]) -> Image<'a> {
        Self::try_validate(bytes).expect("elf: invalid boot module")
    }

    /// Fallible header validation for the `sys_spawn` path — a bad image must be
    /// an error, never a kernel panic (the image came from a userspace request).
    pub fn try_validate(bytes: &'a [u8]) -> Result<Image<'a>, oxbow_abi::SysError> {
        use oxbow_abi::SysError;
        if bytes.len() < core::mem::size_of::<Elf64Ehdr>() {
            return Err(SysError::Msg);
        }
        let eh: Elf64Ehdr = read(bytes, 0);
        if &eh.e_ident[0..4] != b"\x7fELF"
            || eh.e_ident[4] != 2 // ELFCLASS64
            || eh.e_ident[5] != 1 // little-endian
            || eh.e_type != ET_EXEC // no PIE in v0
            || eh.e_machine != EM_X86_64
        {
            return Err(SysError::Msg);
        }
        Ok(Image {
            entry: eh.e_entry,
            bytes,
            phoff: eh.e_phoff as usize,
            phentsize: eh.e_phentsize as usize,
            phnum: eh.e_phnum as usize,
        })
    }

    /// Iterate the PT_LOAD program headers.
    pub fn loads(&self) -> impl Iterator<Item = Elf64Phdr> + '_ {
        (0..self.phnum).filter_map(move |i| {
            let ph: Elf64Phdr = read(self.bytes, self.phoff + i * self.phentsize);
            (ph.p_type == PT_LOAD).then_some(ph)
        })
    }

    /// The PT_TLS program header, if the image has thread-local storage. Describes
    /// the TLS template: `p_vaddr` = where `.tdata` lives in the loaded image,
    /// `p_filesz` = initialized `.tdata` bytes, `p_memsz` = total TLS size
    /// (`.tdata` + `.tbss`), `p_align` = required alignment. Per-thread TLS blocks
    /// are copied from this template (§101 native ELF TLS).
    pub fn tls(&self) -> Option<Elf64Phdr> {
        (0..self.phnum).find_map(|i| {
            let ph: Elf64Phdr = read(self.bytes, self.phoff + i * self.phentsize);
            (ph.p_type == PT_TLS).then_some(ph)
        })
    }

    /// The raw module bytes (for the loader to copy from).
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Validate that the program-header table and every PT_LOAD segment's file
    /// range lie within the byte buffer, and that p_filesz ≤ p_memsz. Boot
    /// modules are trusted (the loader asserts), but an ELF handed to
    /// `sys_spawn_bytes` from a file is untrusted — a truncated or crafted image
    /// must be REJECTED here, not panic the kernel in `load_into`. See ABI §33.
    pub fn segments_in_bounds(&self) -> bool {
        // The phdr table itself must be in bounds before we iterate it.
        let phdr_end = match self.phoff.checked_add(self.phnum.saturating_mul(self.phentsize)) {
            Some(e) => e,
            None => return false,
        };
        if phdr_end > self.bytes.len() {
            return false;
        }
        for ph in self.loads() {
            if ph.p_filesz > ph.p_memsz {
                return false;
            }
            let file_end = match ph.p_offset.checked_add(ph.p_filesz) {
                Some(e) => e,
                None => return false,
            };
            if file_end as usize > self.bytes.len() {
                return false;
            }
        }
        true
    }
}

/// "rwx"-style permission string for a program header's flags.
pub fn perm_str(flags: u32) -> &'static str {
    match (flags & PF_R != 0, flags & PF_W != 0, flags & PF_X != 0) {
        (true, false, true) => "r-x",
        (true, true, false) => "rw-",
        (true, false, false) => "r--",
        _ => "???",
    }
}
