//! PCI configuration space, through the legacy I/O ports (`0xCF8` selects
//! a register, `0xCFC` reads or writes it) -- enough to find a device by
//! its vendor and device IDs, read its BARs and let it do DMA.

use crate::port::{inl, outl};

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Command register bits.
const CMD_IO_SPACE: u16 = 1 << 0;
const CMD_BUS_MASTER: u16 = 1 << 2;

#[derive(Clone, Copy, Debug)]
pub struct Device {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
}

fn address(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    0x8000_0000 | (bus as u32) << 16 | (slot as u32) << 11 | (func as u32) << 8 | (offset as u32 & 0xFC)
}

fn read32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    unsafe {
        outl(CONFIG_ADDRESS, address(bus, slot, func, offset));
        inl(CONFIG_DATA)
    }
}

fn write32(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    unsafe {
        outl(CONFIG_ADDRESS, address(bus, slot, func, offset));
        outl(CONFIG_DATA, value);
    }
}

/// The first device with vendor `vendor` and device `device`, scanning
/// every bus, slot and function.
pub fn find(vendor: u16, device: u16) -> Option<Device> {
    for bus in 0..=255u8 {
        for slot in 0..32u8 {
            let id = read32(bus, slot, 0, 0);
            if id & 0xFFFF == 0xFFFF {
                continue; // Nothing in this slot.
            }
            // Bit 7 of the header type: the device has more functions.
            let multi = read32(bus, slot, 0, 0x0C) >> 16 & 0x80 != 0;
            for func in 0..if multi { 8 } else { 1 } {
                let id = read32(bus, slot, func, 0);
                if id as u16 == vendor && (id >> 16) as u16 == device {
                    return Some(Device { bus, slot, func });
                }
            }
        }
    }
    None
}

impl Device {
    /// Base address register `n` (0-5), raw.
    pub fn bar(&self, n: u8) -> u32 {
        read32(self.bus, self.slot, self.func, 0x10 + 4 * n)
    }

    /// Lets the device answer on its I/O ports and do DMA.
    pub fn enable_io_and_dma(&self) {
        let reg = read32(self.bus, self.slot, self.func, 0x04);
        let cmd = reg as u16 | CMD_IO_SPACE | CMD_BUS_MASTER;
        // The upper half is the status register, whose bits clear when
        // written as 1: write zeros there to leave it alone.
        write32(self.bus, self.slot, self.func, 0x04, cmd as u32);
    }
}
