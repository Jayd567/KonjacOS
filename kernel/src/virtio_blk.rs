//! virtio-blk: the disk QEMU attaches with `-drive ...,if=virtio`.
//!
//! Unlike the ATA PIO driver, where the CPU copies every 16-bit word of
//! every sector through an I/O port and each command moves one sector,
//! the device here reads and writes memory itself (DMA): a request
//! describes up to [`MAX_BYTES`] of data in one go, and the CPU just waits
//! for it to finish.
//!
//! This is the "legacy" virtio interface (registers in an I/O-port BAR),
//! which QEMU's `virtio-blk-pci` offers by default alongside the modern
//! one and which needs no MMIO mappings. One request is in flight at a
//! time and completion is polled -- the callers (the filesystem) are
//! synchronous anyway -- so the device's interrupt stays switched off.
//!
//! A request is a chain of three descriptors in the one virtqueue: a
//! 16-byte header (read or write, starting sector), the data, and a
//! status byte the device fills in. All three live in a fixed DMA area
//! allocated once at start-up; callers' buffers are copied in and out of
//! it, which costs little next to the disk itself and keeps every address
//! the device sees physically contiguous.

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use crate::pci;
use crate::pmm;
use crate::port::{inb, inl, inw, outb, outl, outw};
use crate::sync::IrqSpinLock;

const VENDOR_VIRTIO: u16 = 0x1AF4;
/// The transitional (legacy-capable) virtio block device.
const DEVICE_BLK_LEGACY: u16 = 0x1001;

// Legacy register offsets from the I/O BAR.
const REG_HOST_FEATURES: u16 = 0x00;
const REG_GUEST_FEATURES: u16 = 0x04;
const REG_QUEUE_PFN: u16 = 0x08;
const REG_QUEUE_SIZE: u16 = 0x0C;
const REG_QUEUE_SELECT: u16 = 0x0E;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_STATUS: u16 = 0x12;
/// Device-specific configuration (no MSI-X): the capacity in sectors.
const REG_CAPACITY: u16 = 0x14;

const STATUS_ACKNOWLEDGE: u8 = 1;
const STATUS_DRIVER: u8 = 2;
const STATUS_DRIVER_OK: u8 = 4;
const STATUS_FAILED: u8 = 128;

const FEATURE_RO: u32 = 1 << 5;
const FEATURE_FLUSH: u32 = 1 << 9;

const REQ_IN: u32 = 0; // Read from the disk.
const REQ_OUT: u32 = 1; // Write to the disk.
const REQ_FLUSH: u32 = 4;

const DESC_NEXT: u16 = 1;
/// The device writes into this buffer (rather than reads it).
const DESC_WRITE: u16 = 2;
const AVAIL_NO_INTERRUPT: u16 = 1;

const PAGE: u64 = 4096;
/// The DMA data buffer: the most one request moves.
pub const MAX_BYTES: usize = 64 * 1024;
pub const SECTOR: usize = 512;
/// Status-register polls before a request is declared lost.
const MAX_POLLS: u64 = 500_000_000;

struct Disk {
    io: u16,
    queue_size: u16,
    /// Virtual addresses (through the HHDM) of the queue's three parts.
    desc: *mut u8,
    avail: *mut u8,
    used: *mut u8,
    /// The header + status page, and the data buffer: virtual and physical.
    hdr: *mut u8,
    hdr_phys: u64,
    data: *mut u8,
    data_phys: u64,
    /// `used.idx` as of the last completed request.
    last_used: u16,
    capacity: u64,
    flush: bool,
    read_only: bool,
}

// The raw pointers are into memory only this driver touches, always under
// the lock.
unsafe impl Send for Disk {}

static DISK: IrqSpinLock<Option<Disk>> = IrqSpinLock::new(None);

/// Finds and starts the virtio disk. Returns whether there is one.
pub fn init() -> bool {
    let Some(dev) = pci::find(VENDOR_VIRTIO, DEVICE_BLK_LEGACY) else { return false };
    let bar0 = dev.bar(0);
    if bar0 & 1 == 0 {
        return false; // No I/O BAR: modern-only device, not supported here.
    }
    dev.enable_io_and_dma();
    let io = (bar0 & !0x3) as u16;
    let hhdm = pmm::hhdm_offset();

    unsafe {
        // Reset, then say hello.
        outb(io + REG_STATUS, 0);
        outb(io + REG_STATUS, STATUS_ACKNOWLEDGE);
        outb(io + REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER);

        let offered = inl(io + REG_HOST_FEATURES);
        let wanted = offered & (FEATURE_FLUSH | FEATURE_RO);
        outl(io + REG_GUEST_FEATURES, wanted);

        outw(io + REG_QUEUE_SELECT, 0);
        let qs = inw(io + REG_QUEUE_SIZE);
        if qs < 3 {
            outb(io + REG_STATUS, STATUS_FAILED);
            return false;
        }
        // Legacy layout: descriptors, then the available ring, then (on
        // the next page) the used ring, all physically contiguous.
        let q = qs as u64;
        let avail_off = 16 * q;
        let used_off = (avail_off + 6 + 2 * q).div_ceil(PAGE) * PAGE;
        let total = used_off + (6 + 8 * q).div_ceil(PAGE) * PAGE;
        let pages = total / PAGE;
        let Some(queue_phys) = pmm::alloc_contiguous(pages) else {
            outb(io + REG_STATUS, STATUS_FAILED);
            return false;
        };
        let data_pages = MAX_BYTES as u64 / PAGE;
        let (Some(hdr_phys), Some(data_phys)) = (pmm::alloc_contiguous(1), pmm::alloc_contiguous(data_pages)) else {
            outb(io + REG_STATUS, STATUS_FAILED);
            return false;
        };
        let queue = (queue_phys + hhdm) as *mut u8;
        core::ptr::write_bytes(queue, 0, total as usize);
        core::ptr::write_bytes((hdr_phys + hhdm) as *mut u8, 0, PAGE as usize);

        let avail = queue.add(avail_off as usize);
        // No interrupts: completion is polled.
        write_volatile(avail as *mut u16, AVAIL_NO_INTERRUPT);
        outl(io + REG_QUEUE_PFN, (queue_phys / PAGE) as u32);

        outb(io + REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK);

        let capacity = inl(io + REG_CAPACITY) as u64 | (inl(io + REG_CAPACITY + 4) as u64) << 32;
        *DISK.lock() = Some(Disk {
            io,
            queue_size: qs,
            desc: queue,
            avail,
            used: queue.add(used_off as usize),
            hdr: (hdr_phys + hhdm) as *mut u8,
            hdr_phys,
            data: (data_phys + hhdm) as *mut u8,
            data_phys,
            last_used: 0,
            capacity,
            flush: wanted & FEATURE_FLUSH != 0,
            read_only: wanted & FEATURE_RO != 0,
        });
    }
    true
}

/// The disk's size in sectors (0 if there's no virtio disk).
#[allow(dead_code)] // For the filesystem to size itself against.
pub fn capacity() -> u64 {
    DISK.lock().as_ref().map_or(0, |d| d.capacity)
}

impl Disk {
    /// Writes descriptor `i`.
    unsafe fn set_desc(&mut self, i: usize, addr: u64, len: u32, flags: u16, next: u16) {
        unsafe {
            let d = self.desc.add(16 * i);
            write_volatile(d as *mut u64, addr);
            write_volatile(d.add(8) as *mut u32, len);
            write_volatile(d.add(12) as *mut u16, flags);
            write_volatile(d.add(14) as *mut u16, next);
        }
    }

    /// Runs one request of `kind` at `sector`, moving `len` bytes through
    /// the data buffer (none for a flush), and waits for it.
    fn request(&mut self, kind: u32, sector: u64, len: usize) -> Result<(), &'static str> {
        unsafe {
            // Header: type, reserved, sector. Status byte at offset 16.
            write_volatile(self.hdr as *mut u32, kind);
            write_volatile(self.hdr.add(4) as *mut u32, 0);
            write_volatile(self.hdr.add(8) as *mut u64, sector);
            write_volatile(self.hdr.add(16), 0xFF);

            self.set_desc(0, self.hdr_phys, 16, DESC_NEXT, 1);
            let status_desc = if len > 0 {
                let flags = DESC_NEXT | if kind == REQ_IN { DESC_WRITE } else { 0 };
                self.set_desc(1, self.data_phys, len as u32, flags, 2);
                2
            } else {
                1
            };
            self.set_desc(status_desc, self.hdr_phys + 16, 1, DESC_WRITE, 0);

            // Offer descriptor chain 0 in the available ring.
            let idx = read_volatile(self.avail.add(2) as *const u16);
            let slot = (idx % self.queue_size) as usize;
            write_volatile(self.avail.add(4 + 2 * slot) as *mut u16, 0);
            fence(Ordering::SeqCst);
            write_volatile(self.avail.add(2) as *mut u16, idx.wrapping_add(1));
            fence(Ordering::SeqCst);
            outw(self.io + REG_QUEUE_NOTIFY, 0);

            let mut polls = 0u64;
            loop {
                fence(Ordering::SeqCst);
                let used = read_volatile(self.used.add(2) as *const u16);
                if used != self.last_used {
                    self.last_used = used;
                    break;
                }
                polls += 1;
                if polls > MAX_POLLS {
                    return Err("virtio-blk: the disk stopped answering");
                }
                core::hint::spin_loop();
            }
            // Nothing else raises the interrupt line, but reading the ISR
            // keeps it from staying asserted.
            let _ = inb(self.io + 0x13);
            match read_volatile(self.hdr.add(16)) {
                0 => Ok(()),
                1 => Err("virtio-blk: I/O error"),
                2 => Err("virtio-blk: request not supported"),
                _ => Err("virtio-blk: bad status"),
            }
        }
    }
}

/// Reads `buf.len() / 512` sectors starting at `sector`.
pub fn read(sector: u64, buf: &mut [u8]) -> Result<(), &'static str> {
    let mut guard = DISK.lock();
    let d = guard.as_mut().ok_or("virtio-blk: no disk")?;
    for (k, chunk) in buf.chunks_mut(MAX_BYTES).enumerate() {
        let at = sector + (k * MAX_BYTES / SECTOR) as u64;
        if at + (chunk.len() / SECTOR) as u64 > d.capacity {
            return Err("virtio-blk: past the end of the disk");
        }
        d.request(REQ_IN, at, chunk.len())?;
        unsafe { core::ptr::copy_nonoverlapping(d.data, chunk.as_mut_ptr(), chunk.len()) };
    }
    Ok(())
}

/// Writes `buf.len() / 512` sectors starting at `sector`.
pub fn write(sector: u64, buf: &[u8]) -> Result<(), &'static str> {
    let mut guard = DISK.lock();
    let d = guard.as_mut().ok_or("virtio-blk: no disk")?;
    if d.read_only {
        return Err("virtio-blk: the disk is read-only");
    }
    for (k, chunk) in buf.chunks(MAX_BYTES).enumerate() {
        let at = sector + (k * MAX_BYTES / SECTOR) as u64;
        if at + (chunk.len() / SECTOR) as u64 > d.capacity {
            return Err("virtio-blk: past the end of the disk");
        }
        unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), d.data, chunk.len()) };
        d.request(REQ_OUT, at, chunk.len())?;
    }
    Ok(())
}

/// Makes everything written so far durable (a no-op if the device
/// doesn't cache writes).
#[allow(dead_code)] // For the copy-on-write filesystem's commit points.
pub fn flush() -> Result<(), &'static str> {
    let mut guard = DISK.lock();
    let d = guard.as_mut().ok_or("virtio-blk: no disk")?;
    if d.flush {
        d.request(REQ_FLUSH, 0, 0)?;
    }
    Ok(())
}
