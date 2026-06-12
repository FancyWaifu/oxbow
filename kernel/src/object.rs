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
}

/// What a handle points at. Pool indices are filled in by Phase 8.
#[derive(Clone, Copy)]
#[allow(dead_code)] // Endpoint/Reply payloads are wired up in Phase 8
pub enum ObjectRef {
    Endpoint(u8),
    Reply(u8),
    Console,
}

impl ObjectRef {
    pub fn ty(self) -> ObjType {
        match self {
            ObjectRef::Endpoint(_) => ObjType::Endpoint,
            ObjectRef::Reply(_) => ObjType::Reply,
            ObjectRef::Console => ObjType::Console,
        }
    }
}

/// One slot of a process's handle table: an object reference plus the rights the
/// holder has over it (law L2: rights are per-handle, validated on every use).
#[derive(Clone, Copy)]
pub struct HandleEntry {
    pub obj: ObjectRef,
    pub rights: u32,
}
