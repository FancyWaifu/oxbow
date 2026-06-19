//! CMOS RTC → Unix epoch. The motherboard real-time clock keeps wall-clock date/
//! time across the standard CMOS index/data ports (0x70/0x71). Read it for
//! `SYS_WALLTIME` so `std::time::SystemTime` has a real epoch (the PIT only gives
//! monotonic uptime). One-second granularity; sub-second comes from the PIT.

use x86_64::instructions::port::Port;

unsafe fn cmos_read(reg: u8) -> u8 {
    let mut addr = Port::<u8>::new(0x70);
    let mut data = Port::<u8>::new(0x71);
    // Bit 7 of 0x70 is the NMI-disable line; writing the bare register index
    // leaves NMI enabled, which is what we want for a plain read.
    addr.write(reg);
    data.read()
}

fn update_in_progress() -> bool {
    unsafe { cmos_read(0x0A) & 0x80 != 0 }
}

/// Days from the civil date to 1970-01-01 (Howard Hinnant's algorithm). Valid for
/// any Gregorian date; we only ever feed it post-2000 values.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // Mar=0 … Feb=11
    let doy = (153 * mp + 2) / 5 + (d - 1); // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as i64; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Current wall-clock time as whole seconds since the Unix epoch (UTC). QEMU's RTC
/// defaults to UTC; on real hardware a BIOS set to local time would be offset
/// (a timezone layer is a later concern).
pub fn epoch_secs() -> u64 {
    // Read the time registers; retry until two consecutive snapshots agree so a
    // mid-read RTC tick can't tear the value.
    let snapshot = || {
        while update_in_progress() {}
        unsafe {
            (
                cmos_read(0x00), // seconds
                cmos_read(0x02), // minutes
                cmos_read(0x04), // hours (bit 7 = PM in 12h mode)
                cmos_read(0x07), // day of month
                cmos_read(0x08), // month
                cmos_read(0x09), // year (2 digits)
                cmos_read(0x0B), // status B: bit2=binary(!BCD), bit1=24h(!12h)
            )
        }
    };
    let mut last = snapshot();
    loop {
        let cur = snapshot();
        if cur == last {
            break;
        }
        last = cur;
    }
    let (raw_sec, raw_min, raw_hour, raw_day, raw_month, raw_year, regb) = last;

    let bcd = (regb & 0x04) == 0;
    let dec = |v: u8| if bcd { (v & 0x0F) + ((v >> 4) * 10) } else { v };

    let sec = dec(raw_sec) as u64;
    let min = dec(raw_min) as u64;
    let day = dec(raw_day) as u32;
    let month = dec(raw_month) as u32;
    let year = dec(raw_year) as i64;

    // Hours: extract the 12h PM flag (bit 7) before BCD-decoding the low bits.
    let h12 = (regb & 0x02) == 0;
    let pm = h12 && (raw_hour & 0x80) != 0;
    let mut hour = dec(raw_hour & 0x7F) as u64;
    if h12 {
        if pm {
            if hour != 12 {
                hour += 12;
            }
        } else if hour == 12 {
            hour = 0;
        }
    }

    let full_year = 2000 + year; // 2-digit RTC year; oxbow is a 21st-century OS
    let days = days_from_civil(full_year, month, day);
    (days as u64) * 86400 + hour * 3600 + min * 60 + sec
}
