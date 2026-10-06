//! The data disk, however it's attached: virtio-blk when QEMU provides
//! one (DMA, up to 64 KiB per request -- see `virtio_blk.rs`), otherwise
//! the ATA PIO driver (`ata.rs`, one sector per command). Filesystems
//! read and write runs of sectors through here and never care which.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

use crate::{ata, virtio_blk};

pub const SECTOR_SIZE: usize = 512;

const NONE: u8 = 0;
const ATA: u8 = 1;
const VIRTIO: u8 = 2;

static BACKEND: AtomicU8 = AtomicU8::new(NONE);
static REQUESTS: AtomicU64 = AtomicU64::new(0);
static SECTORS: AtomicU64 = AtomicU64::new(0);

/// Picks the disk driver: virtio if there's a virtio disk, else ATA.
pub fn init() {
    let backend = if virtio_blk::init() { VIRTIO } else { ATA };
    BACKEND.store(backend, Ordering::Relaxed);
}

pub fn backend_name() -> &'static str {
    match BACKEND.load(Ordering::Relaxed) {
        VIRTIO => "virtio-blk, DMA",
        ATA => "ATA PIO",
        _ => "no disk",
    }
}

/// `(requests sent to the disk, sectors moved)` since boot.
pub fn stats() -> (u64, u64) {
    (REQUESTS.load(Ordering::Relaxed), SECTORS.load(Ordering::Relaxed))
}

fn count(requests: u64, bytes: usize) {
    REQUESTS.fetch_add(requests, Ordering::Relaxed);
    SECTORS.fetch_add((bytes / SECTOR_SIZE) as u64, Ordering::Relaxed);
}

/// Reads `buf.len() / 512` sectors starting at `lba` (`buf.len()` must
/// be a multiple of 512).
pub fn read(lba: u64, buf: &mut [u8]) -> Result<(), &'static str> {
    debug_assert!(buf.len() % SECTOR_SIZE == 0);
    match BACKEND.load(Ordering::Relaxed) {
        VIRTIO => {
            virtio_blk::read(lba, buf)?;
            count(buf.len().div_ceil(virtio_blk::MAX_BYTES) as u64, buf.len());
        }
        _ => {
            for (i, chunk) in buf.chunks_exact_mut(SECTOR_SIZE).enumerate() {
                let sector: &mut [u8; SECTOR_SIZE] = chunk.try_into().unwrap();
                unsafe { ata::read_sector(lba as u32 + i as u32, sector) }?;
                count(1, SECTOR_SIZE);
            }
        }
    }
    Ok(())
}

/// Writes `buf.len() / 512` sectors starting at `lba`.
pub fn write(lba: u64, buf: &[u8]) -> Result<(), &'static str> {
    debug_assert!(buf.len() % SECTOR_SIZE == 0);
    match BACKEND.load(Ordering::Relaxed) {
        VIRTIO => {
            virtio_blk::write(lba, buf)?;
            count(buf.len().div_ceil(virtio_blk::MAX_BYTES) as u64, buf.len());
        }
        _ => {
            for (i, chunk) in buf.chunks_exact(SECTOR_SIZE).enumerate() {
                let sector: &[u8; SECTOR_SIZE] = chunk.try_into().unwrap();
                unsafe { ata::write_sector(lba as u32 + i as u32, sector) }?;
                count(1, SECTOR_SIZE);
            }
        }
    }
    Ok(())
}
