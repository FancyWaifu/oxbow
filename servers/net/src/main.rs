//! net — the network driver (resident boot module), the start of the oxbow
//! network stack. This arc proves the PCI/MMIO capability mechanism: net holds a
//! `PciDevice` capability to the one NIC the kernel found (BOOT_PCI) and uses it
//! to read config space, enable the device, map its MMIO BAR0, and read a real
//! hardware register (the e1000's MAC address). The actual TX/RX rings + ethernet
//! /ARP/IP/UDP come in the following arcs.
#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use oxbow_abi::{BOOT_CONSOLE, BOOT_PCI, NET_MMIO};
use oxbow_rt as rt;

fn w(s: &[u8]) {
    let _ = rt::sys_console_write(BOOT_CONSOLE, s.as_ptr(), s.len());
}

/// Read an e1000 MMIO register (byte offset) through the mapped BAR0.
unsafe fn reg(off: usize) -> u32 {
    core::ptr::read_volatile((NET_MMIO as *const u32).add(off / 4))
}

#[no_mangle]
pub extern "C" fn oxbow_main() -> ! {
    // 1. Config space: confirm which device the capability names.
    let id = rt::sys_pci_read(BOOT_PCI, 0x00).unwrap_or(0);
    w(format!("[net] PCI device {:04x}:{:04x}\n", id & 0xFFFF, id >> 16).as_bytes());

    // 2. Enable memory-space decode + bus mastering (command register, bits 1+2).
    let cmd = rt::sys_pci_read(BOOT_PCI, 0x04).unwrap_or(0);
    let _ = rt::sys_pci_write(BOOT_PCI, 0x04, cmd | 0x6);

    // 3. Map the MMIO register BAR (BAR0) into our address space, uncacheable.
    if rt::sys_pci_bar_map(BOOT_PCI, 0, NET_MMIO).is_err() {
        w(b"[net] BAR0 map FAILED\n");
        rt::sys_exit(1);
    }

    // 4. Read a real hardware register through MMIO: the e1000 MAC (RAL/RAH).
    let (ral, rah, status) = unsafe { (reg(0x5400), reg(0x5404), reg(0x0008)) };
    let mac = [
        ral as u8,
        (ral >> 8) as u8,
        (ral >> 16) as u8,
        (ral >> 24) as u8,
        rah as u8,
        (rah >> 8) as u8,
    ];
    w(format!(
        "[net] MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}  STATUS {:#010x}\n",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], status
    )
    .as_bytes());
    w(b"[net] ready (PCI + MMIO capability works)\n");

    // Park: a resident driver. The next arc gives it real RX/TX work + an IRQ.
    let n = rt::sys_notif_create().unwrap_or(0);
    loop {
        let _ = rt::sys_notif_wait(n);
    }
}
