//! Kernel objects and the handle-table entry type.
//!
//! v0 has three object types (ABI §2). Endpoint/Reply carry a pool index whose
//! body lands in Phase 8; Console is a singleton (the serial port as a
//! capability, so even printing obeys law L1 — no ambient "print" syscall).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjType {
    Endpoint,
    Reply,
    Console,
    Memory,
    Frame,
    Notification,
    IoPort,
    Irq,
    Image,
    PciDevice,
    Pipe,
    Framebuffer,
}

/// What a handle points at: a pool index for Endpoint/Reply/Memory/Frame, or the
/// singleton console.
#[derive(Clone, Copy)]
pub enum ObjectRef {
    Endpoint(u8),
    Reply(u8),
    Console,
    Memory(u8),
    Frame(u8),
    Notification(u8),
    IoPort { base: u16, len: u16 },
    Irq(u8),
    /// A spawnable program image, by registry index (see `kernel/src/image.rs`).
    Image(u8),
    /// A single PCI device (bus<<16 | dev<<8 | func) — config-space + BAR access.
    PciDevice(u32),
    /// A kernel-buffered byte pipe, by pool index (see `kernel/src/pipe.rs`).
    Pipe(u8),
    /// The linear framebuffer (singleton): map it + query geometry. Geometry
    /// lives in `kernel/src/fb.rs`; the cap just gates access (law L1).
    Framebuffer,
}

impl ObjectRef {
    pub fn ty(self) -> ObjType {
        match self {
            ObjectRef::Endpoint(_) => ObjType::Endpoint,
            ObjectRef::Reply(_) => ObjType::Reply,
            ObjectRef::Console => ObjType::Console,
            ObjectRef::Memory(_) => ObjType::Memory,
            ObjectRef::Frame(_) => ObjType::Frame,
            ObjectRef::Notification(_) => ObjType::Notification,
            ObjectRef::IoPort { .. } => ObjType::IoPort,
            ObjectRef::Irq(_) => ObjType::Irq,
            ObjectRef::Image(_) => ObjType::Image,
            ObjectRef::PciDevice(_) => ObjType::PciDevice,
            ObjectRef::Pipe(_) => ObjType::Pipe,
            ObjectRef::Framebuffer => ObjType::Framebuffer,
        }
    }
}

/// One slot of a process's handle table: an object reference plus the rights the
/// holder has over it (law L2: rights are per-handle, validated on every use).
#[derive(Clone, Copy)]
pub struct HandleEntry {
    pub obj: ObjectRef,
    pub rights: u32,
    /// Endpoint badge (§14): a server-chosen label the kernel delivers to the
    /// receiver. 0 = unbadged. Set ONCE by `sys_mint` on an unbadged source,
    /// preserved by attenuation and message transfer, never otherwise changed —
    /// that immutability is what makes a delivered badge unforgeable.
    pub badge: u64,
}
