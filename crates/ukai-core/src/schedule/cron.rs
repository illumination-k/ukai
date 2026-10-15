//! Five-field cron patterns (minute hour day-of-month month day-of-week) and
//! the `@hourly`-style aliases: parsing, the next matching minute, and a
//! description in words.
//!
//! As in Vixie cron, when both day fields are restricted a day matching
//! either one matches; when one of them starts with `*`, both must match.

use time::{Date, Duration, OffsetDateTime, PrimitiveDateTime, Time};

/// How far ahead a match is looked for: a 29 February on a given weekday
/// can be 28 years away, and anything rarer never comes.
const SEARCH_DAYS: u32 = 366 * 29;

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
const WEEKDAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAY_NAMES: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// A parsed pattern: the values each field matches, as bit sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    /// Bits 1-31.
    days: u32,
    /// Bits 1-12.
    months: u16,
    /// Bits 0-6, Sunday first.
    weekdays: u8,
    days_restricted: bool,
    weekdays_restricted: bool,
}

/// One field's range of values and the names it accepts, if any.
struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    names: &'static [&'static str],
    /// The first name's value: 1 for months, 0 for weekdays.
    names_from: u32,
}

const MINUTE: Field = Field {
    name: "minute",
    min: 0,
    max: 59,
    names: &[],
    names_from: 0,
};
const HOUR: Field = Field {
    name: "hour",
    min: 0,
    max: 23,
    names: &[],
    names_from: 0,
};
const DAY: Field = Field {
    name: "day of month",
    min: 1,
    max: 31,
    names: &[],
    names_from: 0,
};
const MONTH: Field = Field {
    name: "month",
    min: 1,
    max: 12,
    names: &MONTHS,
    names_from: 1,
};
// 7 is Sunday too; it is folded into 0 after parsing.
const WEEKDAY: Field = Field {
    name: "day of week",
    min: 0,
    max: 7,
    names: &WEEKDAYS,
    names_from: 0,
};

impl Cron {
    /// Parse a pattern; the error says what is wrong in words.
    pub fn parse(pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim();
        let expanded = match pattern.to_ascii_lowercase().as_str() {
            "" => return Err("empty".to_owned()),
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            alias if alias.starts_with('@') => return Err(format!("unknown alias `{pattern}`")),
            _ => pattern,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = fields.as_slice() else {
            return Err(format!(
                "expected 5 fields (minute hour day-of-month month day-of-week), found {}",
                fields.len()
            ));
        };
        let weekdays = parse_field(weekday, &WEEKDAY)?;
        Ok(Self {
            minutes: parse_field(minute, &MINUTE)?,
            hours: u32::try_from(parse_field(hour, &HOUR)?).unwrap_or(0),
            days: u32::try_from(parse_field(day, &DAY)?).unwrap_or(0),
            months: u16::try_from(parse_field(month, &MONTH)?).unwrap_or(0),
            // Sunday is 0 and 7.
            weekdays: u8::try_from((weekdays | (weekdays >> 7)) & 0x7f).unwrap_or(0),
            days_restricted: !day.starts_with('*'),
            weekdays_restricted: !weekday.starts_with('*'),
        })
    }

    fn day_matches(&self, date: Date) -> bool {
        let day = self.days & (1 << date.day()) != 0;
        let weekday = self.weekdays & (1 << date.weekday().number_days_from_sunday()) != 0;
        if self.days_restricted && self.weekdays_restricted {
            day || weekday
        } else {
            day && weekday
        }
    }

    /// The first minute after `after` that matches, in `after`'s offset.
    pub fn next_after(&self, after: OffsetDateTime) -> Option<OffsetDateTime> {
        let offset = after.offset();
        let start = after
            .replace_second(0)
            .ok()?
            .replace_nanosecond(0)
            .ok()?
            .checked_add(Duration::MINUTE)?;
        let mut date = start.date();
        let (mut from_hour, mut from_minute) = (start.hour(), start.minute());
        for _ in 0..SEARCH_DAYS {
            let month = u8::from(date.month());
            if self.months & (1 << month) != 0 && self.day_matches(date) {
                for hour in from_hour..24 {
                    if self.hours & (1 << hour) == 0 {
                        continue;
                    }
                    let first = if hour == from_hour { from_minute } else { 0 };
                    if let Some(minute) = (first..60).find(|m| self.minutes & (1 << m) != 0) {
                        let time = Time::from_hms(hour, minute, 0).ok()?;
                        return Some(PrimitiveDateTime::new(date, time).assume_offset(offset));
                    }
                }
            }
            date = date.next_day()?;
            (from_hour, from_minute) = (0, 0);
        }
        None
    }

    /// The pattern in words, e.g. "At 09:00, Monday through Friday".
    pub fn describe(&self) -> String {
        let minutes = values(self.minutes, 0, 59);
        let hours = values(u64::from(self.hours), 0, 23);
        let every_hour = hours.len() == 24;
        let mut text = match (minutes.as_slice(), hours.as_slice()) {
            (_, _) if minutes.len() == 60 && every_hour => "Every minute".to_owned(),
            ([minute], [hour]) => format!("At {hour:02}:{minute:02}"),
            ([minute], _) if every_hour => format!("At minute {minute} of every hour"),
            ([minute], _) => format!(
                "At minute {minute} past hour {}",
                list(&hours, |v| v.to_string())
            ),
            (_, _) if every_hour => format!("At minutes {}", list(&minutes, |v| v.to_string())),
            (_, _) => format!(
                "At minutes {} past hour {}",
                list(&minutes, |v| v.to_string()),
                list(&hours, |v| v.to_string())
            ),
        };
        let days = values(u64::from(self.days), 1, 31);
        let weekdays = values(u64::from(self.weekdays), 0, 6);
        let day_text = format!("on day {} of the month", list(&days, |v| v.to_string()));
        let weekday_text = list(&weekdays, |v| WEEKDAY_NAMES[v as usize].to_owned());
        let days_shown = days.len() < 31;
        let weekdays_shown = weekdays.len() < 7;
        match (days_shown, weekdays_shown) {
            (true, true) if self.days_restricted && self.weekdays_restricted => {
                text.push_str(&format!(", {day_text} or on {weekday_text}"));
            }
            (true, true) => text.push_str(&format!(", {day_text}, if on {weekday_text}")),
            (true, false) => text.push_str(&format!(", {day_text}")),
            (false, true) => text.push_str(&format!(", {weekday_text}")),
            (false, false) => {}
        }
        let months = values(u64::from(self.months), 1, 12);
        if months.len() < 12 {
            text.push_str(&format!(
                ", in {}",
                list(&months, |v| MONTH_NAMES[v as usize - 1].to_owned())
            ));
        }
        text
    }
}

/// The values set in `bits` between `min` and `max`.
fn values(bits: u64, min: u32, max: u32) -> Vec<u32> {
    (min..=max).filter(|v| bits & (1 << v) != 0).collect()
}

/// `values` with runs of three or more written as ranges.
fn list(values: &[u32], name: impl Fn(u32) -> String) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < values.len() {
        let mut j = i;
        while j + 1 < values.len() && values[j + 1] == values[j] + 1 {
            j += 1;
        }
        if j - i >= 2 {
            parts.push(format!("{} through {}", name(values[i]), name(values[j])));
        } else {
            parts.extend(values[i..=j].iter().map(|v| name(*v)));
        }
        i = j + 1;
    }
    match parts.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The bit set of the values one field matches.
fn parse_field(text: &str, field: &Field) -> Result<u64, String> {
    let mut bits = 0_u64;
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => {
                let step: u32 = step
                    .parse()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or_else(|| format!("bad step `{step}` in the {}", field.name))?;
                (range, step)
            }
            None => (item, 1),
        };
        let (low, high) = if range == "*" {
            (field.min, field.max)
        } else if let Some((low, high)) = range.split_once('-') {
            (value(low, field)?, value(high, field)?)
        } else {
            let low = value(range, field)?;
            // `5/15` runs from 5 to the end, as in Vixie cron.
            (low, if step > 1 { field.max } else { low })
        };
        if low > high {
            return Err(format!("`{range}` runs backwards in the {}", field.name));
        }
        for v in (low..=high).step_by(step as usize) {
            bits |= 1 << v;
        }
    }
    Ok(bits)
}

fn value(text: &str, field: &Field) -> Result<u32, String> {
    let named = field
        .names
        .iter()
        .position(|name| name.eq_ignore_ascii_case(text))
        .and_then(|index| u32::try_from(index).ok())
        .map(|index| index + field.names_from);
    named
        .or_else(|| text.parse().ok())
        .filter(|v| (field.min..=field.max).contains(v))
        .ok_or_else(|| {
            format!(
                "`{text}` is not a {} ({}-{})",
                field.name, field.min, field.max
            )
        })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use time::macros::datetime;

    use super::*;

    #[rstest]
    #[case::weekday_mornings("0 9 * * 1-5", datetime!(2026-10-16 09:30 +9), datetime!(2026-10-19 09:00 +9))]
    #[case::same_hour("*/15 * * * *", datetime!(2026-10-15 10:07:31 UTC), datetime!(2026-10-15 10:15 UTC))]
    #[case::exactly_on_a_match_moves_on("0 9 * * *", datetime!(2026-10-15 09:00 UTC), datetime!(2026-10-16 09:00 UTC))]
    #[case::month_end("0 0 31 * *", datetime!(2026-11-01 00:00 UTC), datetime!(2026-12-31 00:00 UTC))]
    #[case::leap_day("0 0 29 2 *", datetime!(2026-03-01 00:00 UTC), datetime!(2028-02-29 00:00 UTC))]
    #[case::either_day_field("0 0 13 * FRI", datetime!(2026-11-01 00:00 UTC), datetime!(2026-11-06 00:00 UTC))]
    #[case::starred_day_field_must_also_match("0 0 */2 * MON", datetime!(2026-10-15 00:00 UTC), datetime!(2026-10-19 00:00 UTC))]
    #[case::sunday_as_seven("0 12 * * 7", datetime!(2026-10-15 00:00 UTC), datetime!(2026-10-18 12:00 UTC))]
    #[case::alias("@monthly", datetime!(2026-10-15 00:00 UTC), datetime!(2026-11-01 00:00 UTC))]
    #[case::names("30 6 * jan,jul sun", datetime!(2026-10-15 00:00 UTC), datetime!(2027-01-03 06:30 UTC))]
    fn finds_the_next_match(
        #[case] pattern: &str,
        #[case] after: OffsetDateTime,
        #[case] expected: OffsetDateTime,
    ) {
        let cron = Cron::parse(pattern).unwrap_or_else(|e| panic!("{pattern}: {e}"));
        assert_eq!(cron.next_after(after), Some(expected), "{pattern}");
    }

    #[test]
    fn an_impossible_date_never_comes() {
        let cron = Cron::parse("0 0 31 2 *").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(cron.next_after(datetime!(2026-01-01 00:00 UTC)), None);
    }

    #[rstest]
    #[case::seconds("0 0 9 * * *")]
    #[case::too_few("0 9 *")]
    #[case::out_of_range("60 * * * *")]
    #[case::backwards("0 9-5 * * *")]
    #[case::zero_step("*/0 * * * *")]
    #[case::bad_name("0 9 * * FUNDAY")]
    #[case::bad_alias("@sometimes")]
    #[case::empty("  ")]
    fn rejects(#[case] pattern: &str) {
        assert!(Cron::parse(pattern).is_err(), "{pattern}");
    }

    #[rstest]
    #[case("0 9 * * 1-5", "At 09:00, Monday through Friday")]
    #[case("* * * * *", "Every minute")]
    #[case("15 * * * *", "At minute 15 of every hour")]
    #[case("*/20 9-17 * * *", "At minutes 0, 20 and 40 past hour 9 through 17")]
    #[case("0 0 1 1 *", "At 00:00, on day 1 of the month, in January")]
    #[case("0 0 13 * 5", "At 00:00, on day 13 of the month or on Friday")]
    #[case("0 8 * * 0,6", "At 08:00, Sunday and Saturday")]
    fn describes(#[case] pattern: &str, #[case] expected: &str) {
        let cron = Cron::parse(pattern).unwrap_or_else(|e| panic!("{pattern}: {e}"));
        assert_eq!(cron.describe(), expected);
    }
}
