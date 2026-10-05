//! The CMOS real-time clock: the battery-backed wall clock every PC has
//! at I/O ports 0x70/0x71. Read-only here -- the desktop's taskbar clock
//! is the only consumer. QEMU keeps it in UTC unless started with
//! `-rtc base=localtime` (the Makefile does that).

use crate::port::{inb, outb};

const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct DateTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

fn read(reg: u8) -> u8 {
    unsafe {
        // Bit 7 of the index port is the NMI-disable bit; leave NMIs on.
        outb(CMOS_INDEX, reg & 0x7f);
        inb(CMOS_DATA)
    }
}

fn update_in_progress() -> bool {
    read(0x0A) & 0x80 != 0
}

fn read_raw() -> [u8; 7] {
    [read(0x00), read(0x02), read(0x04), read(0x07), read(0x08), read(0x09), read(0x32)]
}

/// The current date and time. The RTC can be caught mid-update, so this
/// reads until two consecutive snapshots agree.
pub fn now() -> DateTime {
    while update_in_progress() {}
    let mut last = read_raw();
    loop {
        while update_in_progress() {}
        let cur = read_raw();
        if cur == last {
            break;
        }
        last = cur;
    }
    let status_b = read(0x0B);
    let bcd = status_b & 0x04 == 0;
    let twelve_hour = status_b & 0x02 == 0;
    let dec = |v: u8| if bcd { (v & 0x0f) + (v >> 4) * 10 } else { v };

    let [sec, min, hour_raw, day, month, year, century] = last;
    let pm = hour_raw & 0x80 != 0;
    let mut hour = dec(hour_raw & 0x7f);
    if twelve_hour {
        hour %= 12;
        if pm {
            hour += 12;
        }
    }
    let century = if century != 0 && dec(century) >= 19 { dec(century) as u16 } else { 20 };
    DateTime {
        year: century * 100 + dec(year) as u16,
        month: dec(month),
        day: dec(day),
        hour,
        minute: dec(min),
        second: dec(sec),
    }
}
