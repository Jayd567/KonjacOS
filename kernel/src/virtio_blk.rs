//! virtio-blk: the disks QEMU attaches with `-drive ...,if=virtio`.
//! Every one found is started, in PCI order; callers pick one by index.
//!
//! Unlike the ATA PIO driver, where the CPU copies every 16-bit word of
//! every sector through an I/O port and each command moves one sector,
//! the device here reads and writes memory itself (DMA): a request
//! describes up to [`MAX_REQUEST`] of data in one go.
//!
//! This is the "legacy" virtio interface (registers in an I/O-port BAR),
//! which QEMU's `virtio-blk-pci` offers by default alongside the modern
//! one and which needs no MMIO mappings. Up to [`SLOTS`] requests are in
//! flight at once and completion is polled -- the callers (the
//! filesystems) wait for their data anyway, and `read_many` lets them
//! work on each piece as it arrives -- so the device's interrupt stays
//! switched off.
//!
//! A request is a chain of descriptors in the one virtqueue: a 16-byte
//! header (read or write, starting sector), the data, and a status byte
//! the device fills in. The data descriptors point straight at the
//! caller's buffer, one per physically contiguous piece of it (pages of
//! the kernel heap needn't be next to each other in physical memory), so
//! nothing is copied and one request can move up to [`MAX_REQUEST`]. A
//! buffer the driver can't translate goes through a fixed bounce buffer
//! instead, [`MAX_BYTES`] at a time. (Everything used to go through the
//! bounce buffer; the copy cost as much as the disk under QEMU.)

extern crate alloc;

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use crate::paging;
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

const FEATURE_SIZE_MAX: u32 = 1 << 1;
const FEATURE_SEG_MAX: u32 = 1 << 2;
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
/// The bounce buffer, for buffers that can't be handed to the device as
/// they are.
pub const MAX_BYTES: usize = 64 * 1024;
/// The most one request moves straight to or from a caller's buffer.
pub const MAX_REQUEST: usize = 1024 * 1024;
/// Data descriptors per request, at most (the queue also holds the header
/// and the status byte).
const MAX_SEGS: usize = 254;
/// Requests in flight at once. QEMU works on them in parallel, which
/// nearly doubles throughput from an image on a slow host filesystem. Each
/// slot owns a share of the descriptor table and its own header and status
/// byte, [`SLOT_HDR`] bytes apart in the header page.
const SLOTS: usize = 4;
const SLOT_HDR: usize = 32;
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
    /// Data descriptors per request, and bytes per descriptor, the device
    /// accepts.
    seg_max: usize,
    size_max: usize,
}

// The raw pointers are into memory only this driver touches, always under
// the lock.
unsafe impl Send for Disk {}

static DISKS: IrqSpinLock<alloc::vec::Vec<Disk>> = IrqSpinLock::new(alloc::vec::Vec::new());

/// Finds and starts every virtio disk; returns how many there are.
pub fn init() -> usize {
    for dev in pci::find_all(VENDOR_VIRTIO, DEVICE_BLK_LEGACY) {
        if let Some(disk) = start(dev) {
            DISKS.lock().push(disk);
        }
    }
    DISKS.lock().len()
}

fn start(dev: pci::Device) -> Option<Disk> {
    let bar0 = dev.bar(0);
    if bar0 & 1 == 0 {
        return None; // No I/O BAR: modern-only device, not supported here.
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
        let wanted = offered & (FEATURE_FLUSH | FEATURE_RO | FEATURE_SEG_MAX | FEATURE_SIZE_MAX);
        outl(io + REG_GUEST_FEATURES, wanted);

        outw(io + REG_QUEUE_SELECT, 0);
        let qs = inw(io + REG_QUEUE_SIZE);
        if (qs as usize) < SLOTS * 3 {
            outb(io + REG_STATUS, STATUS_FAILED);
            return None;
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
            return None;
        };
        let data_pages = MAX_BYTES as u64 / PAGE;
        let (Some(hdr_phys), Some(data_phys)) = (pmm::alloc_contiguous(1), pmm::alloc_contiguous(data_pages)) else {
            outb(io + REG_STATUS, STATUS_FAILED);
            return None;
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
        // Device config after the capacity: size_max, then seg_max.
        let size_max = if wanted & FEATURE_SIZE_MAX != 0 { inl(io + REG_CAPACITY + 8) as usize } else { MAX_REQUEST };
        let seg_max = if wanted & FEATURE_SEG_MAX != 0 { inl(io + REG_CAPACITY + 12) as usize } else { MAX_SEGS };
        let seg_max = seg_max.clamp(1, MAX_SEGS);
        let size_max = size_max.clamp(SECTOR, MAX_REQUEST);
        Some(Disk {
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
            seg_max,
            size_max,
        })
    }
}

/// Disk `dev`'s size in sectors (0 if there's no such disk).
pub fn capacity(dev: usize) -> u64 {
    DISKS.lock().get(dev).map_or(0, |d| d.capacity)
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
    /// the bounce buffer (none for a flush), and waits for it.
    fn request(&mut self, kind: u32, sector: u64, len: usize) -> Result<(), &'static str> {
        if len == 0 {
            self.request_segs(kind, sector, &[])
        } else {
            self.request_segs(kind, sector, &[(self.data_phys, len as u32)])
        }
    }

    /// Runs one request of `kind` at `sector` whose data is the physical
    /// pieces `segs`, and waits for it.
    fn request_segs(&mut self, kind: u32, sector: u64, segs: &[(u64, u32)]) -> Result<(), &'static str> {
        // Nothing else is in flight when this is used.
        unsafe { self.submit(0, kind, sector, segs) };
        self.complete_one()?.1
    }

    /// Descriptors each request slot owns.
    fn descs_per_slot(&self) -> usize {
        self.queue_size as usize / SLOTS
    }

    /// Data pieces one request can have.
    fn seg_limit(&self) -> usize {
        self.seg_max.min(self.descs_per_slot() - 2)
    }

    /// Hands request `kind` at `sector`, with data `segs`, to the device
    /// in slot `slot` (whose descriptors, header and status byte must be
    /// free), without waiting for it.
    ///
    /// # Safety
    /// `segs` must be physical memory that stays valid until it completes.
    unsafe fn submit(&mut self, slot: usize, kind: u32, sector: u64, segs: &[(u64, u32)]) {
        unsafe {
            // Header: type, reserved, sector. Status byte at offset 16.
            let hdr = self.hdr.add(SLOT_HDR * slot);
            let hdr_phys = self.hdr_phys + (SLOT_HDR * slot) as u64;
            write_volatile(hdr as *mut u32, kind);
            write_volatile(hdr.add(4) as *mut u32, 0);
            write_volatile(hdr.add(8) as *mut u64, sector);
            write_volatile(hdr.add(16), 0xFF);

            let base = slot * self.descs_per_slot();
            self.set_desc(base, hdr_phys, 16, DESC_NEXT, base as u16 + 1);
            let flags = DESC_NEXT | if kind == REQ_IN { DESC_WRITE } else { 0 };
            for (i, &(phys, len)) in segs.iter().enumerate() {
                self.set_desc(base + 1 + i, phys, len, flags, (base + 2 + i) as u16);
            }
            self.set_desc(base + 1 + segs.len(), hdr_phys + 16, 1, DESC_WRITE, 0);

            // Offer the chain in the available ring.
            let idx = read_volatile(self.avail.add(2) as *const u16);
            let at = (idx % self.queue_size) as usize;
            write_volatile(self.avail.add(4 + 2 * at) as *mut u16, base as u16);
            fence(Ordering::SeqCst);
            write_volatile(self.avail.add(2) as *mut u16, idx.wrapping_add(1));
            fence(Ordering::SeqCst);
            outw(self.io + REG_QUEUE_NOTIFY, 0);
        }
    }

    /// Waits for the next request to complete: its slot, and how it went.
    /// The outer error means the device stopped answering altogether.
    fn complete_one(&mut self) -> Result<(usize, Result<(), &'static str>), &'static str> {
        unsafe {
            let mut polls = 0u64;
            loop {
                fence(Ordering::SeqCst);
                if read_volatile(self.used.add(2) as *const u16) != self.last_used {
                    break;
                }
                polls += 1;
                if polls > MAX_POLLS {
                    return Err("virtio-blk: the disk stopped answering");
                }
                core::hint::spin_loop();
            }
            // The used ring's element: the chain's head descriptor, then its length.
            let at = (self.last_used % self.queue_size) as usize;
            let head = read_volatile(self.used.add(4 + 8 * at) as *const u32) as usize;
            self.last_used = self.last_used.wrapping_add(1);
            let slot = (head / self.descs_per_slot()).min(SLOTS - 1);
            // Nothing else raises the interrupt line, but reading the ISR
            // keeps it from staying asserted.
            let _ = inb(self.io + 0x13);
            let status = match read_volatile(self.hdr.add(SLOT_HDR * slot + 16)) {
                0 => Ok(()),
                1 => Err("virtio-blk: I/O error"),
                2 => Err("virtio-blk: request not supported"),
                _ => Err("virtio-blk: bad status"),
            };
            Ok((slot, status))
        }
    }
}

/// Splits up to `len` bytes at virtual address `addr` into at most
/// `limit` physically contiguous pieces the device accepts, in `segs`.
/// Returns the bytes they cover -- a whole number of sectors, 0 if the
/// start can't be translated (the caller then uses the bounce buffer) --
/// and how many pieces.
fn segments(d: &Disk, addr: u64, len: usize, limit: usize, segs: &mut [(u64, u32); MAX_SEGS]) -> (usize, usize) {
    let mut n = 0;
    let mut done = 0;
    let end = len.min(MAX_REQUEST);
    while done < end {
        let va = addr + done as u64;
        let Some(pa) = paging::virt_to_phys(va) else { break };
        let step = ((PAGE - (va & (PAGE - 1))) as usize).min(end - done);
        if n > 0 && segs[n - 1].0 + segs[n - 1].1 as u64 == pa && segs[n - 1].1 as usize + step <= d.size_max {
            segs[n - 1].1 += step as u32;
        } else if n < limit {
            segs[n] = (pa, step as u32);
            n += 1;
        } else {
            break;
        }
        done += step;
    }
    // Whole sectors only: trim the end.
    let mut excess = done % SECTOR;
    done -= excess;
    while excess > 0 {
        let last = &mut segs[n - 1];
        if last.1 as usize > excess {
            last.1 -= excess as u32;
            excess = 0;
        } else {
            excess -= last.1 as usize;
            n -= 1;
        }
    }
    (done, n)
}

/// Moves each `(sector, addr, len)` of `items` to or from the disk, with
/// up to [`SLOTS`] requests in flight, and calls `done(i)` as soon as all
/// of item `i` has arrived -- while the rest are still on their way, so
/// the caller can work on it in the meantime. Data goes straight to or
/// from the caller's memory where it can, through the bounce buffer where
/// it can't. Returns the number of requests.
fn transfer(d: &mut Disk, kind: u32, items: &[(u64, u64, usize)], done: &mut dyn FnMut(usize)) -> Result<u64, &'static str> {
    if items.iter().any(|&(sector, _, len)| sector + (len / SECTOR) as u64 > d.capacity) {
        return Err("virtio-blk: past the end of the disk");
    }
    let limit = d.seg_limit();
    let mut segs = [(0u64, 0u32); MAX_SEGS];
    let mut in_slot: [Option<usize>; SLOTS] = [None; SLOTS];
    let mut in_flight = alloc::vec![0usize; items.len()];
    // The next byte to send: item `next`, offset `off`.
    let (mut next, mut off) = (0, 0);
    let mut requests = 0;
    let mut error = None;
    loop {
        // Fill the free slots.
        while error.is_none() && next < items.len() {
            let Some(slot) = in_slot.iter().position(Option::is_none) else { break };
            let (sector, addr, len) = items[next];
            let at = sector + (off / SECTOR) as u64;
            let (bytes, n) = segments(d, addr + off as u64, len - off, limit, &mut segs);
            if bytes > 0 {
                unsafe { d.submit(slot, kind, at, &segs[..n]) };
                in_slot[slot] = Some(next);
                in_flight[next] += 1;
                off += bytes;
                requests += 1;
            } else if len > off {
                // The bounce buffer is one request at a time.
                if in_slot.iter().any(Option::is_some) {
                    break;
                }
                let bytes = (len - off).min(MAX_BYTES);
                let ptr = (addr + off as u64) as *mut u8;
                unsafe {
                    if kind == REQ_OUT {
                        core::ptr::copy_nonoverlapping(ptr, d.data, bytes);
                    }
                    if let Err(e) = d.request(kind, at, bytes) {
                        error = Some(e);
                        break;
                    }
                    if kind == REQ_IN {
                        core::ptr::copy_nonoverlapping(d.data, ptr, bytes);
                    }
                }
                off += bytes;
                requests += 1;
            }
            if off >= len {
                if in_flight[next] == 0 {
                    done(next);
                }
                next += 1;
                off = 0;
            }
        }
        if in_slot.iter().all(Option::is_none) {
            break;
        }
        let (slot, status) = d.complete_one()?;
        let Some(item) = in_slot[slot].take() else { continue };
        in_flight[item] -= 1;
        match status {
            Err(e) => {
                error.get_or_insert(e);
            }
            // All of it sent, and the last of it back.
            Ok(()) if error.is_none() && in_flight[item] == 0 && item < next => done(item),
            Ok(()) => {}
        }
    }
    match error {
        Some(e) => Err(e),
        None => Ok(requests),
    }
}

/// Reads `buf.len() / 512` sectors of disk `dev` starting at `sector`;
/// returns the number of requests it took.
pub fn read(dev: usize, sector: u64, buf: &mut [u8]) -> Result<u64, &'static str> {
    read_many(dev, &mut [(sector, buf)], &mut |_, _| {})
}

/// Reads each `(sector, buffer)` of `items`, several at once, calling
/// `done(i, &buffer)` as each one arrives (see [`transfer`]).
pub fn read_many(dev: usize, items: &mut [(u64, &mut [u8])], done: &mut dyn FnMut(usize, &[u8])) -> Result<u64, &'static str> {
    let list: alloc::vec::Vec<(u64, u64, usize)> = items.iter_mut().map(|(s, b)| (*s, b.as_mut_ptr() as u64, b.len())).collect();
    let mut guard = DISKS.lock();
    let d = guard.get_mut(dev).ok_or("virtio-blk: no such disk")?;
    // The device has finished writing item `i` when it's reported, and
    // `items` is borrowed mutably for all of this, so handing out a shared
    // view of it is sound.
    transfer(d, REQ_IN, &list, &mut |i| done(i, unsafe { core::slice::from_raw_parts(list[i].1 as *const u8, list[i].2) }))
}

/// Writes `buf.len() / 512` sectors of disk `dev` starting at `sector`;
/// returns the number of requests it took.
pub fn write(dev: usize, sector: u64, buf: &[u8]) -> Result<u64, &'static str> {
    write_many(dev, &[(sector, buf)])
}

/// Writes each `(sector, data)` of `items`, several at once.
pub fn write_many(dev: usize, items: &[(u64, &[u8])]) -> Result<u64, &'static str> {
    let list: alloc::vec::Vec<(u64, u64, usize)> = items.iter().map(|(s, b)| (*s, b.as_ptr() as u64, b.len())).collect();
    let mut guard = DISKS.lock();
    let d = guard.get_mut(dev).ok_or("virtio-blk: no such disk")?;
    if d.read_only {
        return Err("virtio-blk: the disk is read-only");
    }
    transfer(d, REQ_OUT, &list, &mut |_| {})
}

/// Makes everything written so far durable (a no-op if the device
/// doesn't cache writes).
pub fn flush(dev: usize) -> Result<(), &'static str> {
    let mut guard = DISKS.lock();
    let d = guard.get_mut(dev).ok_or("virtio-blk: no such disk")?;
    if d.flush {
        d.request(REQ_FLUSH, 0, 0)?;
    }
    Ok(())
}
