//! `CFNumberFormatter`, `CFDateFormatter` and `CFCalendar`.
//!
//! These are the calls `QSystemLocale::query` is made of, and Qt asks all of
//! them while `QApplication`'s constructor is still running - so an unbound one
//! is not a wrong date later, it is a jump to address zero before the first
//! window.
//!
//! What is answered here, and what is deliberately not:
//!
//! * **Formatting a number works.** Latin digits, `.` for the decimal point and
//!   `,` every three places, which is the English form. A guest formatting a
//!   number for display gets a number rather than an empty string.
//! * **Naming a month does not, unless the locale is English.** Month and
//!   weekday names are locale *data*, it lives in CLDR, and this tree carries
//!   none - so the English tables below are published only when the locale this
//!   tree reports is an English one, and every other locale gets nil. That is
//!   not a gap being hidden: nil is the documented answer for a property a
//!   formatter has no value for, and it is the branch the caller already has.
//!   Qt's is its own compiled-in CLDR data, which is *better* than anything here
//!   could be - so a German guest gets German months by being told nothing,
//!   where it would get English ones by being told the wrong thing.
//!
//! The date *patterns* [`cf::CFDateFormatterGetFormat`] returns are Unicode
//! `LDML` skeletons, because that is what the function returns on a Mac and what
//! a caller parses - Qt converts one into its own format string rather than
//! using it as text.

// The C spellings are the interface.
#![allow(non_snake_case)]

use std::ffi::c_void;

use corefoundation::base::{CFIndex, CFTypeRef};
use objc::class::Class;
use objc::object::Id;

/// The shape each formatter here shares: the object, and its settings.
#[repr(C)]
struct Body<T> {
    isa: *const Class,
    contents: *mut T,
}

/// Register a class whose instances are a [`Body<T>`], with a `-dealloc` that
/// gives the contents back.
fn register<T: 'static>(name: &'static std::ffi::CStr) -> &'static Class {
    let base = corefoundation::base::abstract_class(name, None);
    corefoundation::base::note_type_class(base);
    let concrete = corefoundation::base::register(
        // `__NSCF` + the name, which is how every toll-free class in this tree
        // spells the concrete half of the pair.
        leaked_concrete_name(name),
        Some(base),
        size_of::<Body<T>>(),
        false,
    );
    unsafe extern "C" fn dealloc<T>(receiver: Id, _sel: objc::sel::Sel) {
        // SAFETY: `-dealloc` is reached with the last reference gone, and the
        // receiver is a `Body<T>` whose `contents` came from `Box::into_raw`.
        unsafe {
            let body = receiver as *mut Body<T>;
            if !(*body).contents.is_null() {
                drop(Box::from_raw((*body).contents));
                (*body).contents = std::ptr::null_mut();
            }
            objc::arc::finalize(receiver);
        }
    }
    concrete.replace_method(
        objc::sel::get("dealloc"),
        dealloc::<T> as *const c_void,
        c"v16@0:8",
    );
    concrete
}

/// `NSFoo` -> `__NSCFFoo`, as a name that lives for the process.
///
/// Leaked because [`corefoundation::base::register`] takes a name it keeps: the
/// class table holds the pointer for as long as the class, which is forever.
fn leaked_concrete_name(name: &std::ffi::CStr) -> &'static std::ffi::CStr {
    let text = format!("__NSCF{}", name.to_string_lossy().trim_start_matches("NS"));
    Box::leak(
        std::ffi::CString::new(text)
            .expect("a class name with no NUL")
            .into_boxed_c_str(),
    )
}

/// Allocate an instance of `class` holding `contents`.
fn make<T>(class: &'static Class, contents: T) -> CFTypeRef {
    // SAFETY: `Body<T>` begins with `isa` and is the layout `class` was
    // registered with.
    let body: *mut Body<T> = unsafe { corefoundation::base::allocate(class) };
    if body.is_null() {
        return std::ptr::null();
    }
    // SAFETY: a live instance whose second word is ours to fill in.
    unsafe { (*body).contents = Box::into_raw(Box::new(contents)) };
    body as CFTypeRef
}

/// The settings of `object`, if it is an instance of `class`.
///
/// # Safety
///
/// `object` must be null or an object this runtime allocated.
unsafe fn contents<'a, T>(object: CFTypeRef, class: &'static Class) -> Option<&'a mut T> {
    // SAFETY: the caller's obligation.
    if !unsafe { corefoundation::base::is_kind_of(object, class) } {
        return None;
    }
    // SAFETY: an instance of `class`, whose layout is `Body<T>`.
    unsafe {
        let body = object as *mut Body<T>;
        (!(*body).contents.is_null()).then(|| &mut *(*body).contents)
    }
}

//
// Number formatting
//

/// `CFNumberFormatterStyle`. Apple's values, because a caller passes one in.
mod number_style {
    pub const NONE: i32 = 0;
    pub const DECIMAL: i32 = 1;
    pub const CURRENCY: i32 = 2;
    pub const PERCENT: i32 = 3;
    pub const SCIENTIFIC: i32 = 4;
    pub const CURRENCY_ISO_CODE: i32 = 8;
    pub const CURRENCY_PLURAL: i32 = 9;
    pub const CURRENCY_ACCOUNTING: i32 = 10;
}

struct NumberFormat {
    style: i32,
    /// What [`cf::CFNumberFormatterSetProperty`] was told to use, which a caller
    /// formatting a price in another currency than the locale's sets.
    currency_symbol: Option<String>,
}

impl NumberFormat {
    fn is_currency(&self) -> bool {
        use number_style as s;
        matches!(
            self.style,
            s::CURRENCY | s::CURRENCY_ISO_CODE | s::CURRENCY_PLURAL | s::CURRENCY_ACCOUNTING
        )
    }

    /// `value`, as this formatter's style writes it.
    fn format(&self, value: f64, integral: bool) -> String {
        use number_style as s;
        match self.style {
            // Scientific is the one style whose shape is not "digits with
            // separators", so it is written straight out.
            s::SCIENTIFIC => format!("{value:E}"),
            s::PERCENT => format!("{}%", grouped(value * 100.0, Some(0))),
            _ if self.is_currency() => {
                let symbol = self.currency_symbol.as_deref().unwrap_or("$");
                let amount = grouped(value.abs(), Some(2));
                match value < 0.0 {
                    // Apple writes a negative price as `-$1.23`, and accounting
                    // style as `($1.23)`.
                    true if self.style == s::CURRENCY_ACCOUNTING => {
                        format!("({symbol}{amount})")
                    }
                    true => format!("-{symbol}{amount}"),
                    false => format!("{symbol}{amount}"),
                }
            }
            // `NoStyle` is the unadorned number - no grouping separator, which
            // is what makes it the style a caller uses to ask "what is the digit
            // zero in this locale".
            s::NONE => match integral {
                true => format!("{}", value as i64),
                false => trimmed(value),
            },
            // Decimal, and anything this tree does not distinguish from it.
            s::DECIMAL => match integral {
                true => grouped(value, Some(0)),
                false => grouped(value, None),
            },
            _ => match integral {
                true => grouped(value, Some(0)),
                false => grouped(value, None),
            },
        }
    }
}

/// `value` with `,` every three integer digits, to `fraction` decimal places -
/// or to as many as it takes when `fraction` is `None`.
fn grouped(value: f64, fraction: Option<usize>) -> String {
    let plain = match fraction {
        Some(places) => format!("{:.*}", places, value),
        None => trimmed(value),
    };
    let (sign, digits) = match plain.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", plain.as_str()),
    };
    let (whole, rest) = match digits.split_once('.') {
        Some((whole, rest)) => (whole, Some(rest)),
        None => (digits, None),
    };
    let mut out = String::with_capacity(plain.len() + whole.len() / 3 + 1);
    out.push_str(sign);
    for (seen, ch) in whole.chars().enumerate() {
        if seen > 0 && (whole.len() - seen) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    if let Some(rest) = rest {
        out.push('.');
        out.push_str(rest);
    }
    out
}

/// A float with no trailing zeroes and no exponent, which is how a formatter
/// with no fraction-digit setting writes one.
fn trimmed(value: f64) -> String {
    let text = format!("{value}");
    match text.contains('e') || text.contains('E') {
        // Rust's shortest form goes exponential for very large and very small
        // magnitudes; a number formatter does not.
        true => format!("{value:.6}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string(),
        false => text,
    }
}

//
// Date formatting
//

/// `CFDateFormatterStyle`. Apple's values.
mod date_style {
    pub const NONE: i32 = 0;
    pub const SHORT: i32 = 1;
    pub const MEDIUM: i32 = 2;
    pub const LONG: i32 = 3;
    pub const FULL: i32 = 4;
}

struct DateFormat {
    date: i32,
    time: i32,
}

/// The English month names, long then abbreviated.
const MONTHS: [(&str, &str); 12] = [
    ("January", "Jan"),
    ("February", "Feb"),
    ("March", "Mar"),
    ("April", "Apr"),
    ("May", "May"),
    ("June", "Jun"),
    ("July", "Jul"),
    ("August", "Aug"),
    ("September", "Sep"),
    ("October", "Oct"),
    ("November", "Nov"),
    ("December", "Dec"),
];

/// The English weekday names, long then abbreviated, **from Sunday**.
///
/// Sunday first because that is the order a `CFDateFormatter` weekday array is
/// in, and a caller indexes it with a weekday number where Sunday is 1.
const WEEKDAYS: [(&str, &str); 7] = [
    ("Sunday", "Sun"),
    ("Monday", "Mon"),
    ("Tuesday", "Tue"),
    ("Wednesday", "Wed"),
    ("Thursday", "Thu"),
    ("Friday", "Fri"),
    ("Saturday", "Sat"),
];

impl DateFormat {
    /// The Unicode `LDML` pattern this pair of styles means, which is what
    /// `CFDateFormatterGetFormat` answers with.
    fn pattern(&self) -> String {
        let date = match self.date {
            date_style::SHORT => "M/d/yy",
            date_style::MEDIUM => "MMM d, y",
            date_style::LONG => "MMMM d, y",
            date_style::FULL => "EEEE, MMMM d, y",
            date_style::NONE => "",
            // A style this tree does not know is treated as none, for the reason
            // the module note gives: a caller gets no date part rather than one
            // written in a shape that was guessed at.
            _ => "",
        };
        let time = match self.time {
            date_style::SHORT => "h:mm a",
            date_style::MEDIUM => "h:mm:ss a",
            date_style::LONG => "h:mm:ss a z",
            date_style::FULL => "h:mm:ss a zzzz",
            // As above: no time part.
            _ => "",
        };
        match (date.is_empty(), time.is_empty()) {
            (false, false) => format!("{date}, {time}"),
            (false, true) => date.to_string(),
            (true, false) => time.to_string(),
            (true, true) => String::new(),
        }
    }

    /// `when`, a Unix timestamp, written out in this pair of styles.
    fn format(&self, when: f64) -> String {
        let tm = civil(when);
        let (month_long, month_short) = MONTHS[(tm.tm_mon as usize).min(11)];
        let (day_long, _) = WEEKDAYS[(tm.tm_wday as usize).min(6)];
        let year = tm.tm_year + 1900;
        let date = match self.date {
            date_style::SHORT => format!("{}/{}/{:02}", tm.tm_mon + 1, tm.tm_mday, year % 100),
            date_style::MEDIUM => format!("{month_short} {}, {year}", tm.tm_mday),
            date_style::LONG => format!("{month_long} {}, {year}", tm.tm_mday),
            date_style::FULL => format!("{day_long}, {month_long} {}, {year}", tm.tm_mday),
            date_style::NONE => String::new(),
            // As in `pattern`: an unknown style is none.
            _ => String::new(),
        };
        // Twelve-hour, because every English style is: midnight is `12 AM` and
        // noon is `12 PM`, not `0` and `12`.
        let hour = match tm.tm_hour % 12 {
            0 => 12,
            other => other,
        };
        let half = match tm.tm_hour < 12 {
            true => "AM",
            false => "PM",
        };
        let time = match self.time {
            date_style::SHORT => format!("{hour}:{:02} {half}", tm.tm_min),
            date_style::MEDIUM | date_style::LONG | date_style::FULL => {
                format!("{hour}:{:02}:{:02} {half}", tm.tm_min, tm.tm_sec)
            }
            // As above: no time part.
            _ => String::new(),
        };
        match (date.is_empty(), time.is_empty()) {
            (false, false) => format!("{date} at {time}"),
            (false, true) => date,
            (true, false) => time,
            (true, true) => String::new(),
        }
    }
}

/// A Unix timestamp as local civil time.
///
/// `localtime_r` rather than arithmetic here, because it is the call that knows
/// this host's time zone and its daylight-saving rules - the same answer the
/// guest's own `localtime` would give it.
fn civil(when: f64) -> libc::tm {
    let seconds = when as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: a live `time_t` and a live `tm` for the call to fill in.
    unsafe { libc::localtime_r(&seconds, &mut tm) };
    tm
}

//
// Calendars
//

struct Calendar {
    /// Which weekday a week starts on, 1 being Sunday - see [`first_weekday`].
    first_weekday: CFIndex,
}

/// The day a week starts on in the country this tree reports.
///
/// Derived from the country rather than guessed at, the way
/// `locale::uses_metric` is: which day a week begins on is a fact about a place,
/// not a preference this host has a store for. The listed countries are the ones
/// that start a week on Sunday; everywhere else starts on Monday, which is the
/// ISO 8601 week and the majority by a long way.
fn first_weekday(country: &str) -> CFIndex {
    const SUNDAY: &[&str] = &[
        "US", "CA", "JP", "IL", "KR", "TW", "HK", "MO", "PH", "SG", "TH", "ZA", "BR", "MX", "CO",
        "VE", "PE", "AR", "CL", "DO", "GT", "HN", "NI", "PA", "PR", "SV", "JM", "BS", "BZ", "BO",
        "EG", "SA", "AE", "KW", "QA", "BH", "OM", "JO", "IQ", "LY", "DZ", "MA", "SD", "YE", "SY",
        "IN", "PK", "BD", "NP", "LK", "MM", "KH", "LA", "VN", "ID", "MY", "BN", "CN", "MN",
    ];
    match SUNDAY.contains(&country) {
        true => 1,
        false => 2,
    }
}

//
// The classes
//

fn number_class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let p = *CLASS
        .get_or_init(|| objc::class::ptr(register::<NumberFormat>(c"NSNumberFormatter")) as usize);
    // SAFETY: a class registered above and kept for the process.
    unsafe { objc::class::from_ptr(p as *const Class).expect("a number formatter class") }
}

fn date_class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let p = *CLASS
        .get_or_init(|| objc::class::ptr(register::<DateFormat>(c"NSDateFormatter")) as usize);
    // SAFETY: a class registered above and kept for the process.
    unsafe { objc::class::from_ptr(p as *const Class).expect("a date formatter class") }
}

fn calendar_class() -> &'static Class {
    static CLASS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let p = *CLASS.get_or_init(|| objc::class::ptr(register::<Calendar>(c"NSCalendar")) as usize);
    // SAFETY: a class registered above and kept for the process.
    unsafe { objc::class::from_ptr(p as *const Class).expect("a calendar class") }
}

/// Register the three classes, so that a guest binding `_OBJC_CLASS_$_NSCalendar`
/// gets the same class this module allocates from.
pub fn install() {
    number_class();
    date_class();
    calendar_class();
}

/// The C entry points, and the property keys that go with them.
pub mod cf {
    #![allow(non_snake_case)]

    use corefoundation::base::{CFIndex, CFTypeRef};
    use objc::object::Id;

    use super::{Calendar, DateFormat, MONTHS, NumberFormat, WEEKDAYS};

    /// `CFNumberFormatterRef CFNumberFormatterCreate(CFAllocatorRef,
    /// CFLocaleRef, CFNumberFormatterStyle)`
    ///
    /// The locale is not kept: there is one locale in this process - see
    /// `crate::locale` - so a formatter made for it and a formatter made for the
    /// current one are the same formatter.
    unsafe extern "C" fn CFNumberFormatterCreate(
        _allocator: CFTypeRef,
        _locale: CFTypeRef,
        style: i32,
    ) -> CFTypeRef {
        super::make(
            super::number_class(),
            NumberFormat {
                style,
                currency_symbol: None,
            },
        )
    }

    /// `void CFNumberFormatterSetProperty(CFNumberFormatterRef, CFStringRef key,
    /// CFTypeRef value)`
    ///
    /// The currency symbol is the one property this honours, because it is the
    /// one a caller sets: a price in a currency other than the locale's cannot be
    /// written without it. Any other key is ignored rather than refused - a
    /// formatter setting this tree does not model is a difference in the output,
    /// not a reason to stop the guest.
    ///
    /// # Safety
    ///
    /// Each argument is null or an object this runtime allocated.
    unsafe extern "C" fn CFNumberFormatterSetProperty(
        formatter: CFTypeRef,
        key: CFTypeRef,
        value: CFTypeRef,
    ) {
        // SAFETY: the caller's obligation.
        let Some(format) =
            (unsafe { super::contents::<NumberFormat>(formatter, super::number_class()) })
        else {
            return;
        };
        // SAFETY: as above - an `NSString` or nil.
        let key = unsafe { crate::string::bytes(key as Id) };
        if key.as_slice() != CURRENCY_SYMBOL.as_bytes() {
            return;
        }
        // SAFETY: as above.
        let text = unsafe { crate::string::bytes(value as Id) };
        format.currency_symbol = String::from_utf8(text).ok();
    }

    /// `CFStringRef CFNumberFormatterCreateStringWithNumber(CFAllocatorRef,
    /// CFNumberFormatterRef, CFNumberRef)`
    ///
    /// # Safety
    ///
    /// Each argument is null or an object this runtime allocated.
    unsafe extern "C" fn CFNumberFormatterCreateStringWithNumber(
        _allocator: CFTypeRef,
        formatter: CFTypeRef,
        number: CFTypeRef,
    ) -> CFTypeRef {
        // SAFETY: the caller's obligation.
        let Some(format) =
            (unsafe { super::contents::<NumberFormat>(formatter, super::number_class()) })
        else {
            return std::ptr::null();
        };
        // SAFETY: as above - a `CFNumber` or nil.
        let Some(value) = (unsafe { corefoundation::collections::number(number) }) else {
            return std::ptr::null();
        };
        crate::string::from_str(&format.format(value.as_f64(), !value.is_float())) as CFTypeRef
    }

    /// `CFStringRef CFNumberFormatterCreateStringWithValue(CFAllocatorRef,
    /// CFNumberFormatterRef, CFNumberType, const void *valuePtr)`
    ///
    /// The value is read according to `kind`, which is the whole interface: the
    /// caller passes a pointer to a `double` or an `int` and says which.
    ///
    /// # Safety
    ///
    /// `value` points to a live value of the width `kind` names.
    unsafe extern "C" fn CFNumberFormatterCreateStringWithValue(
        _allocator: CFTypeRef,
        formatter: CFTypeRef,
        kind: i32,
        value: *const std::ffi::c_void,
    ) -> CFTypeRef {
        // SAFETY: the caller's obligation.
        let Some(format) =
            (unsafe { super::contents::<NumberFormat>(formatter, super::number_class()) })
        else {
            return std::ptr::null();
        };
        let Some((width, is_float)) = corefoundation::collections::number_shape(kind) else {
            return std::ptr::null();
        };
        if value.is_null() {
            return std::ptr::null();
        }
        let mut bytes = [0u8; 8];
        // SAFETY: the caller states `value` is a live value of this width, and
        // `number_shape` answers at most eight bytes.
        unsafe { std::ptr::copy_nonoverlapping(value as *const u8, bytes.as_mut_ptr(), width) };
        let number = match (is_float, width) {
            (true, 4) => f32::from_le_bytes(bytes[..4].try_into().unwrap()) as f64,
            (true, _) => f64::from_le_bytes(bytes),
            // Sign-extended from its own width, so a negative `int` is a
            // negative number rather than a very large positive one.
            (false, _) => {
                let raw = i64::from_le_bytes(bytes);
                let shift = 64 - width * 8;
                ((raw << shift) >> shift) as f64
            }
        };
        crate::string::from_str(&format.format(number, !is_float)) as CFTypeRef
    }

    /// `CFDateFormatterRef CFDateFormatterCreate(CFAllocatorRef, CFLocaleRef,
    /// CFDateFormatterStyle dateStyle, CFDateFormatterStyle timeStyle)`
    unsafe extern "C" fn CFDateFormatterCreate(
        _allocator: CFTypeRef,
        _locale: CFTypeRef,
        date: i32,
        time: i32,
    ) -> CFTypeRef {
        super::make(super::date_class(), DateFormat { date, time })
    }

    /// `CFStringRef CFDateFormatterGetFormat(CFDateFormatterRef)`
    ///
    /// A `Get`, so the caller does not own it: the pattern is autoreleased rather
    /// than handed over, because a caller that does not release it must not leak
    /// and one that does must not over-release.
    ///
    /// # Safety
    ///
    /// `formatter` is null or an object this runtime allocated.
    unsafe extern "C" fn CFDateFormatterGetFormat(formatter: CFTypeRef) -> CFTypeRef {
        // SAFETY: the caller's obligation.
        let Some(format) =
            (unsafe { super::contents::<DateFormat>(formatter, super::date_class()) })
        else {
            return std::ptr::null();
        };
        let pattern = crate::string::from_str(&format.pattern()) as CFTypeRef;
        // SAFETY: a fresh object this call owns.
        unsafe { corefoundation::api::CFAutorelease(pattern) }
    }

    /// `CFStringRef CFDateFormatterCreateStringWithDate(CFAllocatorRef,
    /// CFDateFormatterRef, CFDateRef)`
    ///
    /// # Safety
    ///
    /// Each argument is null or an object this runtime allocated.
    unsafe extern "C" fn CFDateFormatterCreateStringWithDate(
        _allocator: CFTypeRef,
        formatter: CFTypeRef,
        date: CFTypeRef,
    ) -> CFTypeRef {
        // SAFETY: the caller's obligation.
        let Some(format) =
            (unsafe { super::contents::<DateFormat>(formatter, super::date_class()) })
        else {
            return std::ptr::null();
        };
        let Some(when) = crate::timer::interval_since_1970(date as Id) else {
            return std::ptr::null();
        };
        crate::string::from_str(&format.format(when)) as CFTypeRef
    }

    /// `CFTypeRef CFDateFormatterCopyProperty(CFDateFormatterRef,
    /// CFDateFormatterKey)`
    ///
    /// The month, weekday and half-day names, and nil for everything else -
    /// including *all* of them when the locale is not an English one, which is
    /// the whole point: see this module's note.
    ///
    /// # Safety
    ///
    /// Each argument is null or an object this runtime allocated.
    unsafe extern "C" fn CFDateFormatterCopyProperty(
        formatter: CFTypeRef,
        key: CFTypeRef,
    ) -> CFTypeRef {
        // SAFETY: the caller's obligation.
        if unsafe { super::contents::<DateFormat>(formatter, super::date_class()) }.is_none() {
            return std::ptr::null();
        }
        if !crate::locale::is_english() {
            return std::ptr::null();
        }
        // SAFETY: as above - an `NSString` or nil.
        let key = unsafe { crate::string::bytes(key as Id) };
        let Ok(key) = std::str::from_utf8(&key) else {
            return std::ptr::null();
        };
        // The standalone symbols are the form a name is written in on its own
        // rather than inside a date. In English they are the same words, so they
        // answer from the same table.
        let names: Vec<&str> = match key {
            MONTH_SYMBOLS | STANDALONE_MONTH_SYMBOLS => {
                MONTHS.iter().map(|(long, _)| *long).collect()
            }
            SHORT_MONTH_SYMBOLS | SHORT_STANDALONE_MONTH_SYMBOLS => {
                MONTHS.iter().map(|(_, short)| *short).collect()
            }
            WEEKDAY_SYMBOLS | STANDALONE_WEEKDAY_SYMBOLS => {
                WEEKDAYS.iter().map(|(long, _)| *long).collect()
            }
            SHORT_WEEKDAY_SYMBOLS | SHORT_STANDALONE_WEEKDAY_SYMBOLS => {
                WEEKDAYS.iter().map(|(_, short)| *short).collect()
            }
            AM_SYMBOL => return crate::string::from_str("AM") as CFTypeRef,
            PM_SYMBOL => return crate::string::from_str("PM") as CFTypeRef,
            _ => return std::ptr::null(),
        };
        let strings: Vec<CFTypeRef> = names
            .iter()
            .map(|name| crate::string::from_str(name) as CFTypeRef)
            .collect();
        let array = corefoundation::collections::new_array(
            &strings,
            corefoundation::callbacks::Policy::from_type_callbacks(),
            false,
        );
        for string in strings {
            // The array retained each; these are this call's own references.
            // SAFETY: a live object made immediately above.
            unsafe { corefoundation::api::CFRelease(string) };
        }
        array
    }

    /// `CFCalendarRef CFCalendarCopyCurrent(void)`
    unsafe extern "C" fn CFCalendarCopyCurrent() -> CFTypeRef {
        super::make(
            super::calendar_class(),
            Calendar {
                first_weekday: super::first_weekday(&crate::locale::country_code()),
            },
        )
    }

    /// `CFIndex CFCalendarGetFirstWeekday(CFCalendarRef)`
    ///
    /// One-based with Sunday as 1, which is the numbering the whole calendar API
    /// uses. Sunday for a calendar this tree did not make, because a zero here is
    /// not a weekday at all and a caller would use it as one.
    ///
    /// # Safety
    ///
    /// `calendar` is null or an object this runtime allocated.
    unsafe extern "C" fn CFCalendarGetFirstWeekday(calendar: CFTypeRef) -> CFIndex {
        // SAFETY: the caller's obligation.
        match unsafe { super::contents::<Calendar>(calendar, super::calendar_class()) } {
            Some(calendar) => calendar.first_weekday,
            None => 1,
        }
    }

    /// `CFStringRef CFLocaleCopyDisplayNameForPropertyValue(CFLocaleRef,
    /// CFLocaleKey, CFStringRef value)`
    ///
    /// Nil, always, and deliberately: a display name is the *translated* name of
    /// a language or a country - "Deutsch" for `de`, "États-Unis" for `US` - and
    /// that is CLDR data this tree does not carry. Nil is the documented answer
    /// for a value the locale cannot name, and a caller falls back to its own
    /// tables; an English name returned for a French locale would not be a
    /// fallback, it would be a wrong answer the caller cannot detect.
    unsafe extern "C" fn CFLocaleCopyDisplayNameForPropertyValue(
        _locale: CFTypeRef,
        _key: CFTypeRef,
        _value: CFTypeRef,
    ) -> CFTypeRef {
        std::ptr::null()
    }

    /// `CFNumberFormatterKey`/`CFDateFormatterKey` names. Each is its own symbol
    /// without the leading `k`, which is how CoreFoundation spells them.
    const CURRENCY_SYMBOL: &str = "kCFNumberFormatterCurrencySymbol";
    const MONTH_SYMBOLS: &str = "kCFDateFormatterMonthSymbols";
    const SHORT_MONTH_SYMBOLS: &str = "kCFDateFormatterShortMonthSymbols";
    const STANDALONE_MONTH_SYMBOLS: &str = "kCFDateFormatterStandaloneMonthSymbols";
    const SHORT_STANDALONE_MONTH_SYMBOLS: &str = "kCFDateFormatterShortStandaloneMonthSymbols";
    const WEEKDAY_SYMBOLS: &str = "kCFDateFormatterWeekdaySymbols";
    const SHORT_WEEKDAY_SYMBOLS: &str = "kCFDateFormatterShortWeekdaySymbols";
    const STANDALONE_WEEKDAY_SYMBOLS: &str = "kCFDateFormatterStandaloneWeekdaySymbols";
    const SHORT_STANDALONE_WEEKDAY_SYMBOLS: &str = "kCFDateFormatterShortStandaloneWeekdaySymbols";
    const AM_SYMBOL: &str = "kCFDateFormatterAMSymbol";
    const PM_SYMBOL: &str = "kCFDateFormatterPMSymbol";

    /// The property-key globals, as `(Mach-O name, value)`.
    ///
    /// Every one is a `CFStringRef` **variable**, so each goes through a cell -
    /// an unresolved one binds to zero and the guest passes address zero as the
    /// key. The value is the key's own name, which is what CoreFoundation's is.
    pub const KEYS: &[(&str, &str)] = &[
        ("_kCFNumberFormatterCurrencySymbol", CURRENCY_SYMBOL),
        ("_kCFDateFormatterMonthSymbols", MONTH_SYMBOLS),
        ("_kCFDateFormatterShortMonthSymbols", SHORT_MONTH_SYMBOLS),
        (
            "_kCFDateFormatterStandaloneMonthSymbols",
            STANDALONE_MONTH_SYMBOLS,
        ),
        (
            "_kCFDateFormatterShortStandaloneMonthSymbols",
            SHORT_STANDALONE_MONTH_SYMBOLS,
        ),
        ("_kCFDateFormatterWeekdaySymbols", WEEKDAY_SYMBOLS),
        (
            "_kCFDateFormatterShortWeekdaySymbols",
            SHORT_WEEKDAY_SYMBOLS,
        ),
        (
            "_kCFDateFormatterStandaloneWeekdaySymbols",
            STANDALONE_WEEKDAY_SYMBOLS,
        ),
        (
            "_kCFDateFormatterShortStandaloneWeekdaySymbols",
            SHORT_STANDALONE_WEEKDAY_SYMBOLS,
        ),
        ("_kCFDateFormatterAMSymbol", AM_SYMBOL),
        ("_kCFDateFormatterPMSymbol", PM_SYMBOL),
    ];

    exports::exports! {
        functions {
            "CFNumberFormatterCreate" => CFNumberFormatterCreate,
            "CFNumberFormatterSetProperty" => CFNumberFormatterSetProperty,
            "CFNumberFormatterCreateStringWithNumber" => CFNumberFormatterCreateStringWithNumber,
            "CFNumberFormatterCreateStringWithValue" => CFNumberFormatterCreateStringWithValue,
            "CFDateFormatterCreate" => CFDateFormatterCreate,
            "CFDateFormatterGetFormat" => CFDateFormatterGetFormat,
            "CFDateFormatterCreateStringWithDate" => CFDateFormatterCreateStringWithDate,
            "CFDateFormatterCopyProperty" => CFDateFormatterCopyProperty,
            "CFCalendarCopyCurrent" => CFCalendarCopyCurrent,
            "CFCalendarGetFirstWeekday" => CFCalendarGetFirstWeekday,
            "CFLocaleCopyDisplayNameForPropertyValue" => CFLocaleCopyDisplayNameForPropertyValue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decimal_style_groups_by_thousands() {
        let format = NumberFormat {
            style: number_style::DECIMAL,
            currency_symbol: None,
        };
        assert_eq!(format.format(1234567.0, true), "1,234,567");
        assert_eq!(format.format(-1234.0, true), "-1,234");
        assert_eq!(format.format(999.0, true), "999");
    }

    /// The case Qt uses to ask "which character is the digit zero here": no
    /// style, the integer zero, and the answer has to be exactly one digit.
    #[test]
    fn no_style_writes_a_bare_number() {
        let format = NumberFormat {
            style: number_style::NONE,
            currency_symbol: None,
        };
        assert_eq!(format.format(0.0, true), "0");
        assert_eq!(format.format(1234.0, true), "1234", "no grouping separator");
    }

    #[test]
    fn a_currency_keeps_two_places_and_its_symbol() {
        let mut format = NumberFormat {
            style: number_style::CURRENCY,
            currency_symbol: None,
        };
        assert_eq!(format.format(1234.5, false), "$1,234.50");
        assert_eq!(format.format(-2.0, true), "-$2.00");
        format.currency_symbol = Some("\u{a3}".to_string());
        assert_eq!(format.format(3.0, true), "\u{a3}3.00");
        format.style = number_style::CURRENCY_ACCOUNTING;
        assert_eq!(format.format(-3.0, true), "(\u{a3}3.00)");
    }

    /// The patterns are what a caller parses, so their shape is the interface.
    #[test]
    fn the_patterns_name_each_style() {
        let pattern = |date, time| DateFormat { date, time }.pattern();
        assert_eq!(pattern(date_style::SHORT, date_style::NONE), "M/d/yy");
        assert_eq!(
            pattern(date_style::FULL, date_style::NONE),
            "EEEE, MMMM d, y"
        );
        assert_eq!(pattern(date_style::NONE, date_style::SHORT), "h:mm a");
        assert_eq!(
            pattern(date_style::MEDIUM, date_style::SHORT),
            "MMM d, y, h:mm a"
        );
        assert_eq!(pattern(date_style::NONE, date_style::NONE), "");
    }

    /// Midnight and noon are the two a twelve-hour clock gets wrong.
    #[test]
    fn twelve_hour_time_names_midnight_and_noon() {
        // 1970-01-01 in UTC, so the test is about the arithmetic rather than
        // about this host's time zone.
        let format = DateFormat {
            date: date_style::NONE,
            time: date_style::SHORT,
        };
        let midnight = civil(0.0);
        let hour = match midnight.tm_hour % 12 {
            0 => 12,
            other => other,
        };
        assert!((1..=12).contains(&hour), "a twelve-hour clock has no zero");
        // And the formatter never writes an hour outside that range.
        for step in 0..24 {
            let text = format.format(step as f64 * 3600.0);
            let written: i32 = text.split(':').next().unwrap().parse().unwrap();
            assert!((1..=12).contains(&written), "{text}");
            assert!(text.ends_with(" AM") || text.ends_with(" PM"), "{text}");
        }
    }

    #[test]
    fn a_week_starts_on_sunday_in_the_states_and_monday_in_europe() {
        assert_eq!(first_weekday("US"), 1);
        assert_eq!(first_weekday("DE"), 2);
        assert_eq!(first_weekday("FR"), 2);
        assert_eq!(first_weekday("JP"), 1);
    }
}
