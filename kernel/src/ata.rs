//! Minimal ATA PIO driver: reads and writes 512-byte sectors on the
//! primary bus's master drive (the disk QEMU attaches as `-hda`, i.e.
//! `ide0-hd0` -- separate from the secondary channel the boot CD-ROM
//! lives on). PIO mode polls status registers instead of using IRQ14,
//! which is slower but much simpler, and plenty fast for a small disk.

use crate::port::{inb, insw, outb, outsw};

// Lazy file faults may run in a different task while ordinary filesystem I/O
// is active. Keep each PIO command/data transaction indivisible on this CPU.
static BUS: crate::sync::IrqSpinLock<()> = crate::sync::IrqSpinLock::new(());

const DATA: u16 = 0x1F0;
const ERROR: u16 = 0x1F1;
const SECTOR_COUNT: u16 = 0x1F2;
const LBA_LO: u16 = 0x1F3;
const LBA_MID: u16 = 0x1F4;
const LBA_HI: u16 = 0x1F5;
const DRIVE_HEAD: u16 = 0x1F6;
const STATUS: u16 = 0x1F7;
const COMMAND: u16 = 0x1F7;

const CMD_READ_SECTORS: u8 = 0x20;
const CMD_WRITE_SECTORS: u8 = 0x30;

const STATUS_ERR: u8 = 1 << 0;
const STATUS_DRQ: u8 = 1 << 3;
const STATUS_DF: u8 = 1 << 5;
const STATUS_BSY: u8 = 1 << 7;

pub const SECTOR_SIZE: usize = 512;

/// Turns a raw status byte from `wait_ready`/`wait_bsy_clear` into a
/// diagnostic message. Previously every failure path printed the same
/// "ATA read error (ERR/DF set)" regardless of cause, which made "no disk
/// image was ever attached to this VM" (a floating bus, `status == 0xFF`
/// -- see `wait_ready`'s own doc comment) look identical to "a disk is
/// attached and it reported a real error", even though the fix for each
/// is completely different (attach `-hda`/check the Makefile's disk
/// target vs. investigate the disk image itself). Distinguishing them
/// costs nothing and turns a confusing report into an actionable one.
fn status_to_message(status: u8) -> &'static str {
    if status == 0xFF {
        "ATA error: no drive responding (floating bus -- is a disk image actually attached as the primary master/ide0 drive? check that disk.img exists and was passed to QEMU)"
    } else if status & STATUS_ERR != 0 && status & STATUS_DF != 0 {
        "ATA error (both ERR and DF set -- the drive reported a command error and a device fault)"
    } else if status & STATUS_DF != 0 {
        "ATA error (DF set -- device fault)"
    } else {
        "ATA error (ERR set -- the drive rejected the command, e.g. the requested sector is past the end of the disk image)"
    }
}

/// How many status-register polls to try before giving up. There's no
/// real drive-side timing spec to hit here -- this just needs to be large
/// enough that a real (slow) drive always finishes well within it, while
/// still bounded so a missing/floating bus (no `-hda` attached) fails
/// fast instead of hanging the boot forever.
const MAX_POLL_ATTEMPTS: u32 = 10_000_000;

/// Busy-waits until BSY clears, then checks for an error condition.
/// Returns `Err` (with the raw status byte, for diagnostics) on ERR/DF, or
/// on a timeout / a floating bus (no drive attached reads back 0xFF).
unsafe fn wait_ready() -> Result<(), u8> {
    unsafe {
        // A fresh command needs a moment before status is meaningful;
        // reading the (unused) alternate-status-ish error port four times
        // is the classic "400ns delay" trick.
        for _ in 0..4 {
            inb(STATUS);
        }
        for _ in 0..MAX_POLL_ATTEMPTS {
            let status = inb(STATUS);
            if status == 0xFF {
                // Floating bus: no drive on this channel at all.
                return Err(status);
            }
            if status & STATUS_BSY != 0 {
                continue;
            }
            if status & (STATUS_ERR | STATUS_DF) != 0 {
                return Err(status);
            }
            if status & STATUS_DRQ != 0 {
                return Ok(());
            }
        }
        Err(0xFF) // Timed out -- treat the same as "no drive".
    }
}

/// Like [`wait_ready`], but for after a write's data has already been
/// pushed: waits for BSY to clear (the drive finishing the commit)
/// without requiring DRQ, since the drive isn't asking for more data at
/// that point.
unsafe fn wait_bsy_clear() -> Result<(), u8> {
    unsafe {
        for _ in 0..MAX_POLL_ATTEMPTS {
            let status = inb(STATUS);
            if status == 0xFF {
                return Err(status);
            }
            if status & STATUS_BSY != 0 {
                continue;
            }
            if status & (STATUS_ERR | STATUS_DF) != 0 {
                return Err(status);
            }
            return Ok(());
        }
        Err(0xFF)
    }
}

/// Reads one 512-byte sector at 28-bit LBA `lba` from the primary master
/// drive into `buf` (must be exactly [`SECTOR_SIZE`] bytes).
///
/// # Safety
/// Talks directly to hardware I/O ports; must not be called concurrently
/// with itself or any other ATA operation (no locking here -- callers are
/// expected to serialize, which a single-threaded kernel does for free).
pub unsafe fn read_sector(lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    let _bus = BUS.lock();
    unsafe {
        // 0xE0 = LBA mode, master drive; top 4 bits of the 28-bit LBA go in
        // the low nibble of this register.
        outb(DRIVE_HEAD, 0xE0 | (((lba >> 24) & 0x0F) as u8));
        outb(ERROR, 0); // "features" -- unused for PIO reads.
        outb(SECTOR_COUNT, 1);
        outb(LBA_LO, (lba & 0xFF) as u8);
        outb(LBA_MID, ((lba >> 8) & 0xFF) as u8);
        outb(LBA_HI, ((lba >> 16) & 0xFF) as u8);
        outb(COMMAND, CMD_READ_SECTORS);

        wait_ready().map_err(status_to_message)?;

        // The controller hands back 256 16-bit words, not 512 bytes --
        // native register width for the data port.
        insw(DATA, buf);

        Ok(())
    }
}

/// Writes one 512-byte sector to 28-bit LBA `lba` on the primary master
/// drive.
///
/// # Safety
/// Same requirements as [`read_sector`]: direct hardware I/O, must not be
/// called concurrently with any other ATA operation. Also, obviously,
/// this actually overwrites data on disk -- `lba` must be a sector the
/// caller means to overwrite (filesystem code is responsible for not
/// clobbering something still in use).
pub unsafe fn write_sector(lba: u32, buf: &[u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    let _bus = BUS.lock();
    unsafe {
        outb(DRIVE_HEAD, 0xE0 | (((lba >> 24) & 0x0F) as u8));
        outb(ERROR, 0);
        outb(SECTOR_COUNT, 1);
        outb(LBA_LO, (lba & 0xFF) as u8);
        outb(LBA_MID, ((lba >> 8) & 0xFF) as u8);
        outb(LBA_HI, ((lba >> 16) & 0xFF) as u8);
        outb(COMMAND, CMD_WRITE_SECTORS);

        // Wait for the drive to want data -- same BSY-clear/DRQ-set
        // condition a read waits on before handing bytes over.
        wait_ready().map_err(status_to_message)?;

        outsw(DATA, buf);

        // Then wait for it to actually finish committing the sector
        // before telling the caller the write is done.
        wait_bsy_clear().map_err(status_to_message)?;

        Ok(())
    }
}
