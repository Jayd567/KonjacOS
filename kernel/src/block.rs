//! The data disks, however they're attached: virtio-blk when QEMU
//! provides them (DMA, up to 1 MiB per request -- see `virtio_blk.rs`),
//! otherwise the one ATA PIO disk (`ata.rs`, one sector per command).
//! Disks are numbered from 0; each filesystem finds its own by what's on
//! it (`fat16::init`, `kfs::mount`), and reads and writes runs of sectors
//! through here without caring how the disk is attached.

use core::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};

use crate::{ata, virtio_blk};

pub const SECTOR_SIZE: usize = 512;

const ATA: u8 = 1;
const VIRTIO: u8 = 2;

static BACKEND: AtomicU8 = AtomicU8::new(ATA);
static COUNT: AtomicUsize = AtomicUsize::new(0);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static SECTORS: AtomicU64 = AtomicU64::new(0);

/// Picks the disk driver: virtio if there are virtio disks, else the ATA
/// disk (assumed present; reading it fails cleanly if it isn't).
pub fn init() {
    let n = virtio_blk::init();
    if n > 0 {
        BACKEND.store(VIRTIO, Ordering::Relaxed);
        COUNT.store(n, Ordering::Relaxed);
    } else {
        BACKEND.store(ATA, Ordering::Relaxed);
        COUNT.store(1, Ordering::Relaxed);
    }
}

/// How many disks there are.
pub fn count() -> usize {
    COUNT.load(Ordering::Relaxed)
}

pub fn backend_name() -> &'static str {
    match BACKEND.load(Ordering::Relaxed) {
        VIRTIO => "virtio-blk, DMA",
        _ => "ATA PIO",
    }
}

/// Disk `disk`'s size in sectors, if the driver knows it.
pub fn capacity(disk: usize) -> Option<u64> {
    match BACKEND.load(Ordering::Relaxed) {
        VIRTIO => Some(virtio_blk::capacity(disk)),
        _ => None,
    }
}

/// `(requests sent to the disks, sectors moved)` since boot.
pub fn stats() -> (u64, u64) {
    (REQUESTS.load(Ordering::Relaxed), SECTORS.load(Ordering::Relaxed))
}

fn count_io(requests: u64, bytes: usize) {
    REQUESTS.fetch_add(requests, Ordering::Relaxed);
    SECTORS.fetch_add((bytes / SECTOR_SIZE) as u64, Ordering::Relaxed);
}

/// Reads `buf.len() / 512` sectors of disk `disk` starting at `lba`
/// (`buf.len()` must be a multiple of 512).
pub fn read(disk: usize, lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
    debug_assert!(buf.len() % SECTOR_SIZE == 0);
    match BACKEND.load(Ordering::Relaxed) {
        VIRTIO => {
            let requests = virtio_blk::read(disk, lba, buf)?;
            count_io(requests, buf.len());
        }
        _ => {
            if disk != 0 {
                return Err("no such disk");
            }
            for (i, chunk) in buf.chunks_exact_mut(SECTOR_SIZE).enumerate() {
                let sector: &mut [u8; SECTOR_SIZE] = chunk.try_into().unwrap();
                unsafe { ata::read_sector(lba as u32 + i as u32, sector) }?;
                count_io(1, SECTOR_SIZE);
            }
        }
    }
    Ok(())
}

/// Reads each `(lba, buffer)` of `items` from disk `disk`, calling
/// `done(i, &buffer)` as each arrives. With virtio several are in flight
/// at once, and `done` runs while the rest are still on their way (KFS
/// checks checksums there); with ATA they go one after another.
pub fn read_many(disk: usize, items: &mut [(u64, &mut [u8])], done: &mut dyn FnMut(usize, &[u8])) -> Result<(), &'static str> {
    match BACKEND.load(Ordering::Relaxed) {
        VIRTIO => {
            let bytes = items.iter().map(|(_, b)| b.len()).sum();
            let requests = virtio_blk::read_many(disk, items, done)?;
            count_io(requests, bytes);
        }
        _ => {
            for (i, (lba, buf)) in items.iter_mut().enumerate() {
                read(disk, *lba, buf)?;
                done(i, buf);
            }
        }
    }
    Ok(())
}

/// Writes each `(lba, data)` of `items` to disk `disk`, several at once
/// where the disk allows.
pub fn write_many(disk: usize, items: &[(u64, &[u8])]) -> Result<(), &'static str> {
    match BACKEND.load(Ordering::Relaxed) {
        VIRTIO => {
            let bytes = items.iter().map(|(_, b)| b.len()).sum();
            let requests = virtio_blk::write_many(disk, items)?;
            count_io(requests, bytes);
        }
        _ => {
            for (lba, buf) in items {
                write(disk, *lba, buf)?;
            }
        }
    }
    Ok(())
}

/// Makes everything written to disk `disk` so far durable: the disk may
/// cache writes, and KFS's commits depend on the order they land in.
pub fn flush(disk: usize) -> Result<(), &'static str> {
    match BACKEND.load(Ordering::Relaxed) {
        VIRTIO => virtio_blk::flush(disk),
        _ if disk != 0 => Err("no such disk"),
        _ => unsafe { ata::flush() },
    }
}

/// Writes `buf.len() / 512` sectors of disk `disk` starting at `lba`.
pub fn write(disk: usize, lba: u64, buf: &[u8]) -> Result<(), &'static str> {
    debug_assert!(buf.len() % SECTOR_SIZE == 0);
    match BACKEND.load(Ordering::Relaxed) {
        VIRTIO => {
            let requests = virtio_blk::write(disk, lba, buf)?;
            count_io(requests, buf.len());
        }
        _ => {
            if disk != 0 {
                return Err("no such disk");
            }
            for (i, chunk) in buf.chunks_exact(SECTOR_SIZE).enumerate() {
                let sector: &[u8; SECTOR_SIZE] = chunk.try_into().unwrap();
                unsafe { ata::write_sector(lba as u32 + i as u32, sector) }?;
                count_io(1, SECTOR_SIZE);
            }
        }
    }
    Ok(())
}
