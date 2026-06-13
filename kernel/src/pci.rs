//! PCI bus access (legacy 0xCF8/0xCFC config mechanism).
//!
//! The kernel is the root of hardware authority: it enumerates the PCI bus at
//! boot, finds devices, and (next phase) hands a driver a capability to ONE
//! device — config-space access + its MMIO BARs — never the whole bus. This is
//! the same model as the IoPort/Irq caps for the keyboard and serial drivers.
use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Compose a config-space address. `off` is dword-aligned (low 2 bits ignored).
fn address(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    0x8000_0000
        | (bus as u32) << 16
        | (dev as u32) << 11
        | (func as u32) << 8
        | (off as u32 & 0xFC)
}

/// Read a 32-bit config-space register of device `bus:dev:func`.
pub fn config_read(bus: u8, dev: u8, func: u8, off: u8) -> u32 {
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(address(bus, dev, func, off));
        Port::<u32>::new(CONFIG_DATA).read()
    }
}

/// Write a 32-bit config-space register.
pub fn config_write(bus: u8, dev: u8, func: u8, off: u8, val: u32) {
    unsafe {
        Port::<u32>::new(CONFIG_ADDRESS).write(address(bus, dev, func, off));
        Port::<u32>::new(CONFIG_DATA).write(val);
    }
}

/// One enumerated PCI function.
#[derive(Clone, Copy)]
pub struct Device {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
}

impl Device {
    /// 24-bit bus:dev:func address, packed for a capability badge/handle.
    pub fn bdf(&self) -> u32 {
        (self.bus as u32) << 16 | (self.dev as u32) << 8 | (self.func as u32)
    }

    /// Read base address register `i` (0..6): the raw 32-bit BAR value.
    pub fn bar(&self, i: u8) -> u32 {
        config_read(self.bus, self.dev, self.func, 0x10 + i * 4)
    }

    /// The interrupt line (config 0x3C, low byte): the legacy PIC IRQ the
    /// firmware routed this function's INTx# pin to. QEMU/SeaBIOS programs it.
    pub fn irq_line(&self) -> u8 {
        config_read(self.bus, self.dev, self.func, 0x3C) as u8
    }

    /// Probe BAR `i`'s size by writing all-ones and reading the mask back, then
    /// restoring it. Returns `(phys_base, size)` for a memory BAR, or (0,0).
    pub fn bar_region(&self, i: u8) -> (u64, u64) {
        let off = 0x10 + i * 4;
        let orig = self.bar(i);
        if orig & 1 != 0 {
            return (0, 0); // an I/O BAR, not memory
        }
        config_write(self.bus, self.dev, self.func, off, 0xFFFF_FFFF);
        let mask = config_read(self.bus, self.dev, self.func, off);
        config_write(self.bus, self.dev, self.func, off, orig);
        let base = (orig & 0xFFFF_FFF0) as u64;
        let size = (!(mask & 0xFFFF_FFF0)).wrapping_add(1) as u64;
        (base, size)
    }
}

/// Read the (vendor, device) at an address, or None if absent.
fn probe(bus: u8, dev: u8, func: u8) -> Option<Device> {
    let id = config_read(bus, dev, func, 0x00);
    let vendor = (id & 0xFFFF) as u16;
    if vendor == 0xFFFF {
        return None;
    }
    let class_reg = config_read(bus, dev, func, 0x08);
    Some(Device {
        bus,
        dev,
        func,
        vendor,
        device: (id >> 16) as u16,
        class: (class_reg >> 24) as u8,
        subclass: (class_reg >> 16) as u8,
    })
}

/// Enumerate the PCI bus, logging each function, and return the first network
/// controller (class 0x02) found — the NIC a future net driver will own.
pub fn enumerate() -> Option<Device> {
    let mut nic = None;
    for bus in 0u16..256 {
        for dev in 0u8..32 {
            for func in 0u8..8 {
                if let Some(d) = probe(bus as u8, dev, func) {
                    crate::println!(
                        "[pci] {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x}",
                        bus, dev, func, d.vendor, d.device, d.class, d.subclass
                    );
                    if d.class == 0x02 && nic.is_none() {
                        nic = Some(d);
                    }
                    // func 0 with no multi-function bit: skip funcs 1..8
                    if func == 0 && config_read(bus as u8, dev, 0, 0x0C) & 0x0080_0000 == 0 {
                        break;
                    }
                }
            }
        }
    }
    nic
}
