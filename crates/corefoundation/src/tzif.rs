//! Compiled zoneinfo (TZif, RFC 8536) files, and the POSIX `TZ` rule in their
//! footer.
//!
//! Read by hand rather than through `localtime_r`: that call answers only for
//! the process-wide `TZ`, and switching it per question is a race with every
//! other thread that asks for local time. The footer matters as much as the
//! table - "slim" databases stop the table at the last rule change and leave
//! every later transition to the rule.

/// One local time type: an offset from UTC, whether it is daylight saving, and
/// its abbreviation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kind {
    /// Seconds east of UTC.
    pub offset: i64,
    pub dst: bool,
    pub abbreviation: String,
}

/// A day of the year in a POSIX rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Day {
    /// `Jn`: 1 to 365, never counting February 29.
    Julian(i64),
    /// `n`: 0 to 365, counting February 29.
    Ordinal(i64),
    /// `Mm.w.d`: weekday `d` (0 is Sunday) of week `w` (5 is the last) of
    /// month `m`.
    Month { month: i64, week: i64, weekday: i64 },
}

/// A transition in a rule: the day, and the local time of day it happens at,
/// which may be negative or past midnight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Change {
    day: Day,
    time: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    standard: Kind,
    /// Daylight time, and when it starts and ends.
    daylight: Option<(Kind, Change, Change)>,
}

#[derive(Clone, Debug)]
pub struct Zone {
    /// Transition instants, in Unix seconds, ascending.
    transitions: Vec<i64>,
    /// The type each transition switches to.
    indices: Vec<u8>,
    kinds: Vec<Kind>,
    footer: Option<Rule>,
}

impl Zone {
    /// A zone that is always at one offset.
    pub fn fixed(offset: i64, abbreviation: &str) -> Self {
        Self {
            transitions: Vec::new(),
            indices: Vec::new(),
            kinds: vec![Kind {
                offset,
                dst: false,
                abbreviation: abbreviation.to_owned(),
            }],
            footer: None,
        }
    }

    /// Parse a TZif file, using its 64-bit data when it has any.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let (header, rest) = Header::read(bytes)?;
        if header.version == 0 {
            return Self::body(&header, rest, 4, None);
        }
        let (header, rest) = Header::read(rest.get(header.len(4)..)?)?;
        let tail = rest.get(header.len(8)..)?;
        let footer = match tail.first() {
            Some(b'\n') => {
                let end = tail[1..].iter().position(|b| *b == b'\n')?;
                let text = std::str::from_utf8(&tail[1..1 + end]).ok()?;
                match text.is_empty() {
                    true => None,
                    false => Some(parse_rule(text)?),
                }
            }
            _ => None,
        };
        Self::body(&header, rest, 8, footer)
    }

    fn body(header: &Header, data: &[u8], width: usize, footer: Option<Rule>) -> Option<Self> {
        let mut at = 0;
        let mut take = |n: usize| {
            let slice = data.get(at..at + n);
            at += n;
            slice
        };
        let times = take(header.times * width)?;
        let transitions = times
            .chunks_exact(width)
            .map(|c| match width {
                4 => i64::from(i32::from_be_bytes(c.try_into().unwrap())),
                _ => i64::from_be_bytes(c.try_into().unwrap()),
            })
            .collect();
        let indices = take(header.times)?.to_vec();
        let records = take(header.kinds * 6)?;
        let chars = take(header.chars)?;
        let kinds = records
            .chunks_exact(6)
            .map(|r| {
                let offset = i64::from(i32::from_be_bytes(r[..4].try_into().unwrap()));
                let from = usize::from(r[5]);
                let text = chars.get(from..)?;
                let end = text.iter().position(|b| *b == 0).unwrap_or(text.len());
                Some(Kind {
                    offset,
                    dst: r[4] != 0,
                    abbreviation: String::from_utf8_lossy(&text[..end]).into_owned(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        if kinds.is_empty() || indices.iter().any(|i| usize::from(*i) >= kinds.len()) {
            return None;
        }
        Some(Self {
            transitions,
            indices,
            kinds,
            footer,
        })
    }

    /// Whether `unix` is past the table, where the footer rule governs.
    fn in_footer(&self, unix: i64) -> Option<&Rule> {
        let past = self.transitions.last().is_none_or(|last| unix >= *last);
        self.footer.as_ref().filter(|_| past)
    }

    /// The local time type in effect at `unix`.
    pub fn at(&self, unix: i64) -> Kind {
        if let Some(rule) = self.in_footer(unix) {
            return rule.at(unix);
        }
        // Before the first transition is type 0, per RFC 8536.
        match self.transitions.partition_point(|t| *t <= unix) {
            0 => self.kinds[0].clone(),
            n => self.kinds[usize::from(self.indices[n - 1])].clone(),
        }
    }

    /// The standard (non-daylight) offset at `unix`.
    pub fn standard_offset(&self, unix: i64) -> i64 {
        let now = self.at(unix);
        if !now.dst {
            return now.offset;
        }
        if let Some(rule) = self.in_footer(unix) {
            return rule.standard.offset;
        }
        let n = self.transitions.partition_point(|t| *t <= unix);
        self.indices[..n]
            .iter()
            .rev()
            .map(|i| &self.kinds[usize::from(*i)])
            .find(|kind| !kind.dst)
            .map_or(now.offset - 3600, |kind| kind.offset)
    }

    /// The first instant after `unix` at which the offset or daylight saving
    /// changes.
    pub fn next_transition(&self, unix: i64) -> Option<i64> {
        let mut before = self.at(unix);
        let from = self.transitions.partition_point(|t| *t <= unix);
        for (t, i) in self.transitions[from..].iter().zip(&self.indices[from..]) {
            let kind = &self.kinds[usize::from(*i)];
            if kind.offset != before.offset || kind.dst != before.dst {
                return Some(*t);
            }
            before = kind.clone();
        }
        let rule = self.footer.as_ref()?;
        let start = unix.max(self.transitions.last().copied().unwrap_or(i64::MIN));
        let year = year_of(start);
        (year..year + 3)
            .flat_map(|y| rule.instants(y))
            .flatten()
            .filter(|(t, _)| *t > unix)
            .min_by_key(|(t, _)| *t)
            .map(|(t, _)| t)
    }
}

struct Header {
    version: u8,
    utc: usize,
    std: usize,
    leaps: usize,
    times: usize,
    kinds: usize,
    chars: usize,
}

impl Header {
    fn read(bytes: &[u8]) -> Option<(Self, &[u8])> {
        if bytes.get(..4)? != b"TZif" {
            return None;
        }
        let count = |i: usize| -> Option<usize> {
            let at = 20 + i * 4;
            Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?) as usize)
        };
        let header = Self {
            version: *bytes.get(4)?,
            utc: count(0)?,
            std: count(1)?,
            leaps: count(2)?,
            times: count(3)?,
            kinds: count(4)?,
            chars: count(5)?,
        };
        Some((header, bytes.get(44..)?))
    }

    /// The data block's length for `width`-byte times.
    fn len(&self, width: usize) -> usize {
        self.times * (width + 1)
            + self.kinds * 6
            + self.chars
            + self.leaps * (width + 4)
            + self.std
            + self.utc
    }
}

impl Rule {
    /// The instants daylight time starts and ends in `year`, each with whether
    /// it is the start.
    fn instants(&self, year: i64) -> [Option<(i64, bool)>; 2] {
        let Some((daylight, start, end)) = &self.daylight else {
            return [None, None];
        };
        // A start is written in standard local time, an end in daylight time.
        let begin = start.local(year) - self.standard.offset;
        let finish = end.local(year) - daylight.offset;
        [Some((begin, true)), Some((finish, false))]
    }

    fn at(&self, unix: i64) -> Kind {
        let Some((daylight, ..)) = &self.daylight else {
            return self.standard.clone();
        };
        let year = year_of(unix);
        let latest = (year - 1..=year + 1)
            .flat_map(|y| self.instants(y))
            .flatten()
            .filter(|(t, _)| *t <= unix)
            .max_by_key(|(t, _)| *t);
        match latest {
            Some((_, true)) => daylight.clone(),
            _ => self.standard.clone(),
        }
    }
}

impl Change {
    /// Seconds from 1970-01-01T00:00 local to this change in `year`.
    fn local(&self, year: i64) -> i64 {
        let jan1 = days_from_civil(year, 1, 1);
        let day = match self.day {
            Day::Julian(n) => jan1 + n - 1 + i64::from(is_leap(year) && n >= 60),
            Day::Ordinal(n) => jan1 + n,
            Day::Month {
                month,
                week,
                weekday,
            } => {
                let first = days_from_civil(year, month, 1);
                // 1970-01-01 was a Thursday.
                let first_weekday = (first + 4).rem_euclid(7);
                let mut date = first + (weekday - first_weekday).rem_euclid(7) + (week - 1) * 7;
                let next_month = match month {
                    12 => days_from_civil(year + 1, 1, 1),
                    _ => days_from_civil(year, month + 1, 1),
                };
                while date >= next_month {
                    date -= 7;
                }
                date
            }
        };
        day * 86_400 + self.time
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn year_of(unix: i64) -> i64 {
    crate::calendar::civil_from_days(unix.div_euclid(86_400)).0
}

use crate::calendar::days_from_civil;

/// Parse a POSIX `TZ` string: `std offset [dst [offset] [,start[/time],end[/time]]]`.
fn parse_rule(text: &str) -> Option<Rule> {
    let mut p = Parser(text.as_bytes());
    let standard_name = p.name()?;
    // POSIX offsets count west of UTC; a `Kind` counts east.
    let standard = Kind {
        offset: -p.time()?,
        dst: false,
        abbreviation: standard_name,
    };
    if p.0.is_empty() {
        return Some(Rule {
            standard,
            daylight: None,
        });
    }
    let daylight_name = p.name()?;
    let offset = match p.0.first() {
        Some(b',') | None => standard.offset + 3600,
        Some(_) => -p.time()?,
    };
    let daylight = Kind {
        offset,
        dst: true,
        abbreviation: daylight_name,
    };
    // No rule given: the US rule, which is what glibc and tzcode assume.
    let (start, end) = match p.0.is_empty() {
        true => (
            Change {
                day: Day::Month {
                    month: 3,
                    week: 2,
                    weekday: 0,
                },
                time: 7200,
            },
            Change {
                day: Day::Month {
                    month: 11,
                    week: 1,
                    weekday: 0,
                },
                time: 7200,
            },
        ),
        false => {
            p.expect(b',')?;
            let start = p.change()?;
            p.expect(b',')?;
            let end = p.change()?;
            (start, end)
        }
    };
    p.0.is_empty().then_some(Rule {
        standard,
        daylight: Some((daylight, start, end)),
    })
}

struct Parser<'a>(&'a [u8]);

impl Parser<'_> {
    fn expect(&mut self, byte: u8) -> Option<()> {
        let (first, rest) = self.0.split_first()?;
        (*first == byte).then(|| self.0 = rest)
    }

    /// An abbreviation: three or more letters, or anything inside `<>`.
    fn name(&mut self) -> Option<String> {
        let (text, rest) = match self.0.first()? {
            b'<' => {
                let end = self.0.iter().position(|b| *b == b'>')?;
                (&self.0[1..end], &self.0[end + 1..])
            }
            _ => {
                let end = self
                    .0
                    .iter()
                    .position(|b| !b.is_ascii_alphabetic())
                    .unwrap_or(self.0.len());
                (&self.0[..end], &self.0[end..])
            }
        };
        if text.len() < 3 {
            return None;
        }
        self.0 = rest;
        Some(String::from_utf8_lossy(text).into_owned())
    }

    fn number(&mut self) -> Option<i64> {
        let end = self
            .0
            .iter()
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(self.0.len());
        let value = std::str::from_utf8(&self.0[..end]).ok()?.parse().ok()?;
        self.0 = &self.0[end..];
        Some(value)
    }

    /// `[+-]hh[:mm[:ss]]`, in seconds.
    fn time(&mut self) -> Option<i64> {
        let sign = match self.0.first() {
            Some(b'-') => {
                self.0 = &self.0[1..];
                -1
            }
            Some(b'+') => {
                self.0 = &self.0[1..];
                1
            }
            _ => 1,
        };
        let mut seconds = self.number()? * 3600;
        for scale in [60, 1] {
            if self.expect(b':').is_none() {
                break;
            }
            seconds += self.number()? * scale;
        }
        Some(sign * seconds)
    }

    fn change(&mut self) -> Option<Change> {
        let day = match self.0.first()? {
            b'J' => {
                self.0 = &self.0[1..];
                Day::Julian(self.number().filter(|n| (1..=365).contains(n))?)
            }
            b'M' => {
                self.0 = &self.0[1..];
                let month = self.number().filter(|n| (1..=12).contains(n))?;
                self.expect(b'.')?;
                let week = self.number().filter(|n| (1..=5).contains(n))?;
                self.expect(b'.')?;
                let weekday = self.number().filter(|n| (0..=6).contains(n))?;
                Day::Month {
                    month,
                    week,
                    weekday,
                }
            }
            _ => Day::Ordinal(self.number().filter(|n| (0..=365).contains(n))?),
        };
        let time = match self.expect(b'/') {
            Some(()) => self.time()?,
            None => 7200,
        };
        Some(Change { day, time })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unix(y: i64, m: i64, d: i64, h: i64) -> i64 {
        days_from_civil(y, m, d) * 86_400 + h * 3600
    }

    #[test]
    fn a_us_rule_switches_on_the_second_sunday_of_march() {
        let rule = parse_rule("EST5EDT,M3.2.0,M11.1.0").expect("a rule");
        // 2026-03-08 is the second Sunday; 02:00 EST is 07:00 UTC.
        assert_eq!(rule.at(unix(2026, 3, 8, 6)).abbreviation, "EST");
        let summer = rule.at(unix(2026, 3, 8, 7));
        assert_eq!((summer.offset, summer.dst), (-4 * 3600, true));
        // 2026-11-01, 02:00 EDT is 06:00 UTC.
        assert!(rule.at(unix(2026, 11, 1, 5)).dst);
        assert!(!rule.at(unix(2026, 11, 1, 6)).dst);
    }

    #[test]
    fn a_southern_rule_is_in_daylight_time_over_new_year() {
        // Australia/Sydney.
        let rule = parse_rule("AEST-10AEDT,M10.1.0,M4.1.0/3").expect("a rule");
        let january = rule.at(unix(2026, 1, 15, 0));
        assert_eq!((january.offset, january.dst), (11 * 3600, true));
        assert!(!rule.at(unix(2026, 7, 1, 0)).dst);
    }

    #[test]
    fn quoted_names_and_fixed_zones_parse() {
        let rule = parse_rule("<+0545>-5:45").expect("a rule");
        assert_eq!(rule.standard.offset, 5 * 3600 + 45 * 60);
        assert_eq!(rule.standard.abbreviation, "+0545");
        assert!(rule.daylight.is_none());
        assert!(parse_rule("EST5EDT,M3.2.0").is_none());
    }

    /// A table-less zone with a footer - the shape of a slim file - finds its
    /// next transition from the rule.
    #[test]
    fn the_next_transition_comes_from_the_footer() {
        let zone = Zone {
            transitions: Vec::new(),
            indices: Vec::new(),
            kinds: vec![Kind {
                offset: -5 * 3600,
                dst: false,
                abbreviation: "EST".into(),
            }],
            footer: parse_rule("EST5EDT,M3.2.0,M11.1.0"),
        };
        let from = unix(2026, 1, 1, 0);
        assert_eq!(zone.next_transition(from), Some(unix(2026, 3, 8, 7)));
        assert_eq!(zone.standard_offset(unix(2026, 7, 1, 0)), -5 * 3600);
    }

    /// The host's own database, when it has one, parses and agrees with
    /// the rule every US zone follows.
    #[test]
    fn a_host_zone_file_parses() {
        let Some(bytes) = ["/usr/share/zoneinfo", "/etc/zoneinfo"]
            .iter()
            .find_map(|dir| std::fs::read(format!("{dir}/America/New_York")).ok())
        else {
            return;
        };
        let zone = Zone::parse(&bytes).expect("a TZif file");
        let summer = zone.at(unix(2026, 7, 1, 12));
        assert_eq!((summer.offset, summer.dst), (-4 * 3600, true));
        assert_eq!(summer.abbreviation, "EDT");
        assert_eq!(zone.at(unix(2026, 1, 1, 12)).offset, -5 * 3600);
        // Before 1883 New York kept local mean time.
        assert_eq!(zone.at(unix(1800, 1, 1, 0)).abbreviation, "LMT");
        assert_eq!(
            zone.next_transition(unix(2026, 1, 1, 0)),
            Some(unix(2026, 3, 8, 7))
        );
    }
}
