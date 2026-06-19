//! oxbow std::fs — real File open/read/write/seek + stat over the fsd protocol
//! (oxbow-rt's `__oxbow_fs_*` shims), relative to the program's cwd dir cap.
//! Directory listing, symlinks, rename/unlink, timestamps and permissions are not
//! wired yet (stubbed) — this milestone is whole-file + positioned File I/O.
use crate::ffi::OsString;
use crate::fmt;
use crate::fs::TryLockError;
use crate::hash::{Hash, Hasher};
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, SeekFrom};
use crate::path::{Path, PathBuf};
pub use crate::sys::fs::common::Dir;
use crate::sync::atomic::{AtomicU64, Ordering};
use crate::sys::time::SystemTime;
use crate::sys::unsupported;

unsafe extern "C" {
    fn __oxbow_fs_open(
        path: *const u8,
        path_len: usize,
        create: i32,
        size_out: *mut u64,
        is_dir_out: *mut i32,
    ) -> i64;
    fn __oxbow_fs_pread(file: i64, buf: *mut u8, len: usize, off: u64) -> isize;
    fn __oxbow_fs_pwrite(file: i64, buf: *const u8, len: usize, off: u64) -> isize;
    fn __oxbow_fs_close(file: i64);
    fn __oxbow_fs_mkdir(path: *const u8, len: usize) -> i32;
    fn __oxbow_fs_readdir(dir: i64, cursor: u64, name_out: *mut u8, name_cap: usize, kind_out: *mut u32) -> isize;
    fn __oxbow_fs_unlink(path: *const u8, len: usize) -> i32;
    fn __oxbow_fs_rename(old: *const u8, old_len: usize, new: *const u8, new_len: usize) -> i32;
}

const FS_DIR: u32 = 1; // oxbow_abi::FS_DIR

#[inline]
fn pbytes(p: &Path) -> &[u8] {
    p.as_os_str().as_encoded_bytes()
}
fn ioerr(kind: io::ErrorKind, msg: &'static str) -> io::Error {
    io::Error::new(kind, msg)
}

pub struct File {
    fd: i64,
    pos: AtomicU64,
    size: AtomicU64,
    is_dir: bool,
}

#[derive(Clone)]
pub struct FileAttr {
    size: u64,
    is_dir: bool,
}

pub struct ReadDir {
    cap: i64,
    cursor: u64,
    base: PathBuf,
}
impl Drop for ReadDir {
    fn drop(&mut self) {
        unsafe { __oxbow_fs_close(self.cap) };
    }
}

#[derive(Clone)]
pub struct DirEntry {
    name: OsString,
    kind: FileType,
    base: PathBuf,
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct FileTimes {}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FilePermissions;

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileType {
    is_dir: bool,
}

#[derive(Debug)]
pub struct DirBuilder {}

impl FileAttr {
    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn perm(&self) -> FilePermissions {
        FilePermissions
    }
    pub fn file_type(&self) -> FileType {
        FileType { is_dir: self.is_dir }
    }
    pub fn modified(&self) -> io::Result<SystemTime> {
        unsupported()
    }
    pub fn accessed(&self) -> io::Result<SystemTime> {
        unsupported()
    }
    pub fn created(&self) -> io::Result<SystemTime> {
        unsupported()
    }
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        false
    }
    pub fn set_readonly(&mut self, _readonly: bool) {}
}

impl FileTimes {
    pub fn set_accessed(&mut self, _t: SystemTime) {}
    pub fn set_modified(&mut self, _t: SystemTime) {}
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }
    pub fn is_file(&self) -> bool {
        !self.is_dir
    }
    pub fn is_symlink(&self) -> bool {
        false
    }
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadDir").field("base", &self.base).finish()
    }
}
impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;
    fn next(&mut self) -> Option<io::Result<DirEntry>> {
        loop {
            let mut buf = [0u8; 64];
            let mut kind = 0u32;
            let n = unsafe {
                __oxbow_fs_readdir(self.cap, self.cursor, buf.as_mut_ptr(), buf.len(), &mut kind)
            };
            self.cursor += 1;
            if n < 0 {
                return None;
            }
            let name = OsString::from(String::from_utf8_lossy(&buf[..n as usize]).into_owned());
            if name == "." || name == ".." {
                continue; // std::fs::read_dir omits . and ..
            }
            return Some(Ok(DirEntry {
                name,
                kind: FileType { is_dir: kind == FS_DIR },
                base: self.base.clone(),
            }));
        }
    }
}
impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.base.join(&self.name)
    }
    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }
    pub fn metadata(&self) -> io::Result<FileAttr> {
        stat(&self.path())
    }
    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.kind)
    }
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }
    pub fn read(&mut self, read: bool) {
        self.read = read;
    }
    pub fn write(&mut self, write: bool) {
        self.write = write;
    }
    pub fn append(&mut self, append: bool) {
        self.append = append;
    }
    pub fn truncate(&mut self, truncate: bool) {
        self.truncate = truncate;
    }
    pub fn create(&mut self, create: bool) {
        self.create = create;
    }
    pub fn create_new(&mut self, create_new: bool) {
        self.create_new = create_new;
    }
}

impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let b = pbytes(path);
        // fsd has open (existing) and create-or-truncate. Map: any create flag ->
        // create-or-truncate; otherwise open the existing node.
        let create = (opts.create || opts.create_new) as i32;
        let mut size = 0u64;
        let mut is_dir = 0i32;
        let fd = unsafe { __oxbow_fs_open(b.as_ptr(), b.len(), create, &mut size, &mut is_dir) };
        if fd < 0 {
            return Err(ioerr(io::ErrorKind::NotFound, "oxbow fs: open failed"));
        }
        let start = if opts.append { size } else { 0 };
        Ok(File {
            fd,
            pos: AtomicU64::new(start),
            size: AtomicU64::new(size),
            is_dir: is_dir != 0,
        })
    }

    pub fn file_attr(&self) -> io::Result<FileAttr> {
        Ok(FileAttr { size: self.size.load(Ordering::Relaxed), is_dir: self.is_dir })
    }
    pub fn fsync(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn datasync(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn lock(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn lock_shared(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn try_lock(&self) -> Result<(), TryLockError> {
        Ok(())
    }
    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        Ok(())
    }
    pub fn unlock(&self) -> io::Result<()> {
        Ok(())
    }
    pub fn truncate(&self, _size: u64) -> io::Result<()> {
        unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let off = self.pos.load(Ordering::Relaxed);
        let n = unsafe { __oxbow_fs_pread(self.fd, buf.as_mut_ptr(), buf.len(), off) };
        if n < 0 {
            return Err(ioerr(io::ErrorKind::Other, "oxbow fs: read failed"));
        }
        self.pos.store(off + n as u64, Ordering::Relaxed);
        Ok(n as usize)
    }
    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        for b in bufs {
            if !b.is_empty() {
                return self.read(b);
            }
        }
        Ok(0)
    }
    pub fn is_read_vectored(&self) -> bool {
        false
    }
    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_>) -> io::Result<()> {
        let mut tmp = [0u8; 512];
        let want = cursor.capacity().min(tmp.len());
        let n = self.read(&mut tmp[..want])?;
        cursor.append(&tmp[..n]);
        Ok(())
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let off = self.pos.load(Ordering::Relaxed);
        let n = unsafe { __oxbow_fs_pwrite(self.fd, buf.as_ptr(), buf.len(), off) };
        if n < 0 {
            return Err(ioerr(io::ErrorKind::Other, "oxbow fs: write failed"));
        }
        let end = off + n as u64;
        self.pos.store(end, Ordering::Relaxed);
        let _ = self.size.fetch_max(end, Ordering::Relaxed);
        Ok(n as usize)
    }
    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        for b in bufs {
            if !b.is_empty() {
                return self.write(b);
            }
        }
        Ok(0)
    }
    pub fn is_write_vectored(&self) -> bool {
        false
    }
    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn seek(&self, pos: SeekFrom) -> io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(o) => o,
            SeekFrom::End(o) => (self.size.load(Ordering::Relaxed) as i64 + o) as u64,
            SeekFrom::Current(o) => (self.pos.load(Ordering::Relaxed) as i64 + o) as u64,
        };
        self.pos.store(new, Ordering::Relaxed);
        Ok(new)
    }
    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(Ok(self.size.load(Ordering::Relaxed)))
    }
    pub fn tell(&self) -> io::Result<u64> {
        Ok(self.pos.load(Ordering::Relaxed))
    }
    pub fn duplicate(&self) -> io::Result<File> {
        unsupported()
    }
    pub fn set_permissions(&self, _perm: FilePermissions) -> io::Result<()> {
        Ok(())
    }
    pub fn set_times(&self, _times: FileTimes) -> io::Result<()> {
        unsupported()
    }
}

impl Drop for File {
    fn drop(&mut self) {
        unsafe { __oxbow_fs_close(self.fd) };
    }
}

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder {}
    }
    pub fn mkdir(&self, p: &Path) -> io::Result<()> {
        let b = pbytes(p);
        if unsafe { __oxbow_fs_mkdir(b.as_ptr(), b.len()) } != 0 {
            return Err(ioerr(io::ErrorKind::Other, "oxbow fs: mkdir failed"));
        }
        Ok(())
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File").field("fd", &self.fd).finish()
    }
}

pub fn stat(p: &Path) -> io::Result<FileAttr> {
    let b = pbytes(p);
    let mut size = 0u64;
    let mut is_dir = 0i32;
    let fd = unsafe { __oxbow_fs_open(b.as_ptr(), b.len(), 0, &mut size, &mut is_dir) };
    if fd < 0 {
        return Err(ioerr(io::ErrorKind::NotFound, "oxbow fs: stat not found"));
    }
    unsafe { __oxbow_fs_close(fd) };
    Ok(FileAttr { size, is_dir: is_dir != 0 })
}
pub fn lstat(p: &Path) -> io::Result<FileAttr> {
    stat(p)
}
pub fn exists(p: &Path) -> io::Result<bool> {
    Ok(stat(p).is_ok())
}

pub fn readdir(p: &Path) -> io::Result<ReadDir> {
    let b = pbytes(p);
    let mut size = 0u64;
    let mut is_dir = 0i32;
    let cap = unsafe { __oxbow_fs_open(b.as_ptr(), b.len(), 0, &mut size, &mut is_dir) };
    if cap < 0 {
        return Err(ioerr(io::ErrorKind::NotFound, "oxbow fs: read_dir not found"));
    }
    if is_dir == 0 {
        unsafe { __oxbow_fs_close(cap) };
        return Err(ioerr(io::ErrorKind::NotADirectory, "oxbow fs: not a directory"));
    }
    Ok(ReadDir { cap, cursor: 0, base: p.to_path_buf() })
}
pub fn unlink(p: &Path) -> io::Result<()> {
    let b = pbytes(p);
    if unsafe { __oxbow_fs_unlink(b.as_ptr(), b.len()) } != 0 {
        return Err(ioerr(io::ErrorKind::Other, "oxbow fs: remove_file failed"));
    }
    Ok(())
}
pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    let (o, n) = (pbytes(old), pbytes(new));
    if unsafe { __oxbow_fs_rename(o.as_ptr(), o.len(), n.as_ptr(), n.len()) } != 0 {
        return Err(ioerr(io::ErrorKind::Other, "oxbow fs: rename failed"));
    }
    Ok(())
}
pub fn set_perm(_p: &Path, _perm: FilePermissions) -> io::Result<()> {
    Ok(())
}
pub fn set_times(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}
pub fn set_times_nofollow(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}
pub fn rmdir(p: &Path) -> io::Result<()> {
    // fsd's UNLINK (oxfs_remove) falls back to ext4_dir_rm, so it removes an
    // (empty) directory too.
    let b = pbytes(p);
    if unsafe { __oxbow_fs_unlink(b.as_ptr(), b.len()) } != 0 {
        return Err(ioerr(io::ErrorKind::Other, "oxbow fs: remove_dir failed"));
    }
    Ok(())
}
pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    for entry in readdir(path)? {
        let entry = entry?;
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            remove_dir_all(&p)?;
        } else {
            unlink(&p)?;
        }
    }
    rmdir(path)
}
pub fn readlink(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}
pub fn symlink(_original: &Path, _link: &Path) -> io::Result<()> {
    unsupported()
}
pub fn link(_src: &Path, _dst: &Path) -> io::Result<()> {
    unsupported()
}
pub fn canonicalize(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}
pub fn copy(from: &Path, to: &Path) -> io::Result<u64> {
    crate::sys::fs::common::copy(from, to)
}
