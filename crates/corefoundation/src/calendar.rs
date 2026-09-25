use crate::base::CFTypeRef;

/// `CFAbsoluteTime`: seconds since 2001-01-01 00:00:00 UTC.
type CFAbsoluteTime = f64;

/// Seconds from the Unix epoch to CoreFoundation's.
const UNIX_TO_ABSOLUTE: i64 = 978_307_200;

/// The identifiers `<CFLocale.h>` publishes, as the strings they are. Each
/// constant *is* its own text, which is why the same list serves both as the
/// published globals and as what [`create_with_identifier`] accepts.
pub const IDENTIFIERS: &[(&str, &str)] = &[
    ("_kCFGregorianCalendar", "gregorian"),
    ("_kCFBuddhistCalendar", "buddhist"),
    ("_kCFChineseCalendar", "chinese"),
    ("_kCFHebrewCalendar", "hebrew"),
    ("_kCFIslamicCalendar", "islamic"),
    ("_kCFIslamicCivilCalendar", "islamic-civil"),
    ("_kCFJapaneseCalendar", "japanese"),
    ("_kCFRepublicOfChinaCalendar", "roc"),
    ("_kCFPersianCalendar", "persian"),
    ("_kCFIndianCalendar", "indian"),
    ("_kCFISO8601Calendar", "iso8601"),
];

/// `CFGregorianDate`, whose layout is ABI: a guest declares one on its stack
/// and this is returned in registers by value.
///
/// Four narrow integers and a `double`, which puts the first eightbyte in an
/// integer register and the second in a vector one - the same classification
/// Apple's declaration gets.
#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub struct GregorianDate {
    pub year: i32,
    pub month: i8,
    pub day: i8,
    pub hour: i8,
    pub minute: i8,
    pub second: f64,
}

const _: () = {
    assert!(size_of::<GregorianDate>() == 16);
    assert!(std::mem::offset_of!(GregorianDate, second) == 8);
};

/// The zone's offset from UTC at `at`, in seconds.
///
/// Only two zones can be answered: UTC, and whichever one this host is set to -
/// which is what `CFTimeZoneCopySystem` hands back and what a program asking
/// for "now, locally" means. Any other named zone would need the zone database
/// read by hand, and reporting this machine's offset for it would be a wrong
/// time rather than a refusal.
///
/// # Safety
///
/// `timezone` is null or an object.
pub unsafe fn offset_seconds(timezone: CFTypeRef, at: CFAbsoluteTime) -> Option<i64> {
    // SAFETY: null or a time zone this tree made.
    let name = match unsafe { crate::timezone::name_of(timezone) } {
        // No zone at all means UTC, which is what CoreFoundation does with a
        // null `CFTimeZoneRef` in these functions.
        None => return Some(0),
        Some(name) => name,
    };
    if name == "UTC" || name == "GMT" || name == "Etc/UTC" || name == "Etc/GMT" {
        return Some(0);
    }
    if name != crate::timezone::system_name() {
        objc::fail!(
            "CFCalendar: this runtime can place a date in UTC and in this machine's own zone, \
             and {name} is neither"
        );
    }
    let unix = (at as i64).checked_add(UNIX_TO_ABSOLUTE)?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time = unix as libc::time_t;
    // SAFETY: `localtime_r` fills the `tm` it is given and reads only `time`.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return None;
    }
    Some(tm.tm_gmtoff)
}

/// Days from 1970-01-01 to `y-m-d`, by Howard Hinnant's closed form.
///
/// Written out rather than taken from a library because it is eight lines and
/// because the two directions have to be exact inverses of each other.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    (y + i64::from(m <= 2), m, d)
}

/// `CFGregorianDate CFAbsoluteTimeGetGregorianDate(CFAbsoluteTime, CFTimeZoneRef)`
pub unsafe extern "C" fn absolute_time_get_gregorian_date(
    at: CFAbsoluteTime,
    timezone: CFTypeRef,
) -> GregorianDate {
    let Some(offset) = (unsafe { offset_seconds(timezone, at) }) else {
        return GregorianDate::default();
    };
    // The fractional second is kept apart from the civil arithmetic, which is
    // whole seconds: `floor` rather than a cast, so that a time before the
    // epoch does not round towards it.
    let local = at + offset as f64;
    let whole = local.floor();
    let fraction = local - whole;
    let seconds = whole as i64 + UNIX_TO_ABSOLUTE;
    let days = seconds.div_euclid(86_400);
    let within = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    GregorianDate {
        year: year as i32,
        month: month as i8,
        day: day as i8,
        hour: (within / 3600) as i8,
        minute: (within % 3600 / 60) as i8,
        second: (within % 60) as f64 + fraction,
    }
}

/// `CFAbsoluteTime CFGregorianDateGetAbsoluteTime(CFGregorianDate, CFTimeZoneRef)`
pub unsafe extern "C" fn gregorian_date_get_absolute_time(
    date: GregorianDate,
    timezone: CFTypeRef,
) -> CFAbsoluteTime {
    let days = days_from_civil(
        i64::from(date.year),
        i64::from(date.month),
        i64::from(date.day),
    );
    let local = (days * 86_400 + i64::from(date.hour) * 3600 + i64::from(date.minute) * 60
        - UNIX_TO_ABSOLUTE) as f64
        + date.second;
    // The offset is the one in force at that moment, which is why it is asked
    // for with the answer rather than before it. Local time is not UTC, so the
    // first reading can be off by an hour across a daylight-saving change; it
    // is corrected by asking again at the time it named.
    let Some(guess) = (unsafe { offset_seconds(timezone, local) }) else {
        return local;
    };
    let Some(offset) = (unsafe { offset_seconds(timezone, local - guess as f64) }) else {
        return local;
    };
    local - offset as f64
}

/// `SInt32 CFAbsoluteTimeGetDayOfWeek(CFAbsoluteTime, CFTimeZoneRef)`
///
/// One for Monday through seven for Sunday, which is CoreFoundation's
/// numbering and not the C library's.
pub unsafe extern "C" fn absolute_time_get_day_of_week(
    at: CFAbsoluteTime,
    timezone: CFTypeRef,
) -> i32 {
    let Some(offset) = (unsafe { offset_seconds(timezone, at) }) else {
        return 0;
    };
    let seconds = (at + offset as f64).floor() as i64 + UNIX_TO_ABSOLUTE;
    // 1970-01-01 was a Thursday, which is day four.
    (seconds.div_euclid(86_400) + 3).rem_euclid(7) as i32 + 1
}

/// `SInt32 CFAbsoluteTimeGetDayOfYear(CFAbsoluteTime, CFTimeZoneRef)`
pub unsafe extern "C" fn absolute_time_get_day_of_year(
    at: CFAbsoluteTime,
    timezone: CFTypeRef,
) -> i32 {
    let date = unsafe { absolute_time_get_gregorian_date(at, timezone) };
    let day = days_from_civil(
        i64::from(date.year),
        i64::from(date.month),
        i64::from(date.day),
    );
    let first = days_from_civil(i64::from(date.year), 1, 1);
    (day - first) as i32 + 1
}

exports::exports! {
    functions {
        "CFAbsoluteTimeGetGregorianDate" => absolute_time_get_gregorian_date,
        "CFGregorianDateGetAbsoluteTime" => gregorian_date_get_absolute_time,
        "CFAbsoluteTimeGetDayOfWeek" => absolute_time_get_day_of_week,
        "CFAbsoluteTimeGetDayOfYear" => absolute_time_get_day_of_year,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The epoch itself, and a date either side of it, in UTC - which is the
    /// arithmetic everything else here is built on.
    #[test]
    fn the_absolute_epoch_is_the_first_of_2001() {
        // SAFETY: a null time zone means UTC.
        let date = unsafe { absolute_time_get_gregorian_date(0.0, std::ptr::null()) };
        assert_eq!(
            date,
            GregorianDate {
                year: 2001,
                month: 1,
                day: 1,
                hour: 0,
                minute: 0,
                second: 0.0,
            }
        );
        let later = unsafe { absolute_time_get_gregorian_date(760_000_000.25, std::ptr::null()) };
        assert_eq!(later.year, 2025);
        assert!((later.second.fract() - 0.25).abs() < 1e-9);
    }

    /// The two directions are inverses, which is what a program that stores a
    /// date and reads it back depends on.
    #[test]
    fn a_date_survives_the_round_trip() {
        for at in [-1.0, 0.0, 1.5, 123_456_789.0, 900_000_000.5] {
            // SAFETY: a null time zone means UTC on both sides.
            let (there, back) = unsafe {
                let date = absolute_time_get_gregorian_date(at, std::ptr::null());
                (
                    date,
                    gregorian_date_get_absolute_time(date, std::ptr::null()),
                )
            };
            assert!(
                (back - at).abs() < 1e-6,
                "{at} came back as {back} via {there:?}"
            );
        }
    }

    /// Monday is one and Sunday is seven, which is not the C library's
    /// numbering and is the mistake this test exists to catch.
    #[test]
    fn the_week_starts_on_monday() {
        // 2001-01-01 was a Monday.
        // SAFETY: a null time zone means UTC.
        assert_eq!(
            unsafe { absolute_time_get_day_of_week(0.0, std::ptr::null()) },
            1
        );
        // ...so the day before it was a Sunday.
        assert_eq!(
            unsafe { absolute_time_get_day_of_week(-1.0, std::ptr::null()) },
            7
        );
        assert_eq!(
            unsafe { absolute_time_get_day_of_year(0.0, std::ptr::null()) },
            1
        );
    }

    /// A day before the Unix epoch, which is where a sign error in the civil
    /// arithmetic shows up.
    #[test]
    fn a_date_before_the_unix_epoch_is_placed() {
        let at = -(UNIX_TO_ABSOLUTE as f64) - 86_400.0;
        // SAFETY: a null time zone means UTC.
        let date = unsafe { absolute_time_get_gregorian_date(at, std::ptr::null()) };
        assert_eq!(
            (date.year, date.month, date.day),
            (1969, 12, 31),
            "{date:?}"
        );
    }
}
