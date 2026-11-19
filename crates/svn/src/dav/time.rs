//! Subversion's dates on the wire (§4.17.4 piece 13; A-609): the civil date of a count of days since 1970 and back,
//! for the form `svn_time_to_cstring` writes, `YYYY-MM-DDTHH:MM:SS.uuuuuuZ` (`libsvn_subr/time.c`
//! `TIMESTAMP_FORMAT`), in UTC. The arithmetic is Howard Hinnant's `days_from_civil` and `civil_from_days`
//! (<http://howardhinnant.github.io/date_algorithms.html>), on unsigned numbers since only the years 1970 to 9999 are
//! read or written, every step checked.

/// Seconds in a day.
/// Format: UTC days of 86,400 seconds, as `apr_time_exp_gmt` counts them (no leap seconds).
pub(crate) const SECONDS_PER_DAY: u64 = 86_400;

/// Seconds in an hour.
/// Format: 60 × 60.
const SECONDS_PER_HOUR: u64 = 3_600;

/// Seconds in a minute, and minutes in an hour.
/// Format: the clock's sixty.
const SIXTY: u64 = 60;

/// Hours in a day.
/// Format: the clock's twenty-four.
const HOURS_PER_DAY: u64 = 24;

/// The first year read or written.
/// Format: the Unix epoch's year.
const FIRST_YEAR: u64 = 1970;

/// The last year read or written.
/// Format: the last year `%04d` writes in four digits.
const LAST_YEAR: u64 = 9999;

/// Days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar.
/// Format: Hinnant's `days_from_civil` offset, 719,468.
const EPOCH_OFFSET: u64 = 719_468;

/// Days in a 400-year era of the Gregorian calendar.
/// Format: 400 × 365 + 97 leap days.
const ERA_DAYS: u64 = 146_097;

/// Years in an era, and the period of the leap rule's last exception.
/// Format: the Gregorian calendar's 400.
const ERA_YEARS: u64 = 400;

/// Days in a common year.
/// Format: the Gregorian calendar's 365.
const COMMON_YEAR: u64 = 365;

/// The leap rule's period.
/// Format: every 4th year (the Gregorian calendar).
const LEAP_EVERY: u64 = 4;

/// The leap rule's exception.
/// Format: but not every 100th year (the Gregorian calendar).
const CENTURY: u64 = 100;

/// `civil_from_days`'s corrections of a day of era for its 4-year, 100-year and 400-year leap days.
/// Format: Hinnant's `doe / 1460`.
const CORRECTION_FOUR: u64 = 1_460;

/// See [`CORRECTION_FOUR`].
/// Format: Hinnant's `doe / 36524`.
const CORRECTION_HUNDRED: u64 = 36_524;

/// See [`CORRECTION_FOUR`].
/// Format: Hinnant's `doe / 146096`.
const CORRECTION_ERA: u64 = 146_096;

/// The month rule of a year counted from March: days before month `m` are `(153 m + 2) / 5`.
/// Format: Hinnant's 153.
const MONTH_MULTIPLIER: u64 = 153;

/// See [`MONTH_MULTIPLIER`].
/// Format: Hinnant's 2.
const MONTH_ADDEND: u64 = 2;

/// See [`MONTH_MULTIPLIER`].
/// Format: Hinnant's 5.
const MONTH_DIVISOR: u64 = 5;

/// Months in a year.
/// Format: twelve.
const MONTHS_PER_YEAR: u64 = 12;

/// March, the first month of a year counted from March.
/// Format: month 3.
const MARCH: u64 = 3;

/// The shift from a month counted from January to one counted from March: `mp = (m + 9) % 12`.
/// Format: Hinnant's 9.
const MARCH_SHIFT: u64 = 9;

/// The month counted from March that is January: `m = mp < 10 ? mp + 3 : mp - 9`.
/// Format: Hinnant's 10.
const JANUARY_FROM_MARCH: u64 = 10;

/// The first day of a month, and the first month of a year.
/// Format: dates count from 1.
const FIRST: u64 = 1;

/// The month lengths of a common year, January first.
/// Format: the Gregorian calendar.
const MONTH_LENGTHS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// February.
/// Format: month 2.
const FEBRUARY: u64 = 2;

/// Days since 1970-01-01 of a date, `None` past the years read or for a day its month does not hold.
pub(crate) fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
  let in_range =
    (FIRST_YEAR..=LAST_YEAR).contains(&year) && (FIRST..=MONTHS_PER_YEAR).contains(&month);
  if !in_range || day < FIRST || day > days_in_month(year, month)? {
    return None;
  }
  let shifted_year = if month < MARCH {
    year.checked_sub(FIRST)?
  } else {
    year
  };
  let era = shifted_year / ERA_YEARS;
  let year_of_era = shifted_year.checked_sub(era.checked_mul(ERA_YEARS)?)?;
  let month_from_march = month.checked_add(MARCH_SHIFT)? % MONTHS_PER_YEAR;
  let month_start = MONTH_MULTIPLIER
    .checked_mul(month_from_march)?
    .checked_add(MONTH_ADDEND)?
    / MONTH_DIVISOR;
  let day_of_year = month_start.checked_add(day)?.checked_sub(FIRST)?;
  let day_of_era = year_of_era
    .checked_mul(COMMON_YEAR)?
    .checked_add(year_of_era / LEAP_EVERY)?
    .checked_sub(year_of_era / CENTURY)?
    .checked_add(day_of_year)?;
  era
    .checked_mul(ERA_DAYS)?
    .checked_add(day_of_era)?
    .checked_sub(EPOCH_OFFSET)
}

/// The date of a count of days since 1970-01-01, `None` past the years written.
pub(crate) fn civil_from_days(days: u64) -> Option<(u64, u64, u64)> {
  let shifted = days.checked_add(EPOCH_OFFSET)?;
  let era = shifted / ERA_DAYS;
  let day_of_era = shifted.checked_sub(era.checked_mul(ERA_DAYS)?)?;
  let (year_of_era, day_of_year) = year_of_era(day_of_era)?;
  let (month, day) = month_and_day(day_of_year)?;
  let year = year_of_era.checked_add(era.checked_mul(ERA_YEARS)?)?;
  let year = if month < MARCH {
    year.checked_add(FIRST)?
  } else {
    year
  };
  (FIRST_YEAR..=LAST_YEAR)
    .contains(&year)
    .then_some((year, month, day))
}

/// The year of its 400-year era a day of that era falls in, and the day's place in that year counted from March 1.
fn year_of_era(day_of_era: u64) -> Option<(u64, u64)> {
  let year_of_era = day_of_era
    .checked_sub(day_of_era / CORRECTION_FOUR)?
    .checked_add(day_of_era / CORRECTION_HUNDRED)?
    .checked_sub(day_of_era / CORRECTION_ERA)?
    / COMMON_YEAR;
  let year_start = COMMON_YEAR
    .checked_mul(year_of_era)?
    .checked_add(year_of_era / LEAP_EVERY)?
    .checked_sub(year_of_era / CENTURY)?;
  Some((year_of_era, day_of_era.checked_sub(year_start)?))
}

/// The month (January 1) and day of the month of a day counted from March 1.
fn month_and_day(day_of_year: u64) -> Option<(u64, u64)> {
  let month_from_march = MONTH_DIVISOR
    .checked_mul(day_of_year)?
    .checked_add(MONTH_ADDEND)?
    / MONTH_MULTIPLIER;
  let month_start = MONTH_MULTIPLIER
    .checked_mul(month_from_march)?
    .checked_add(MONTH_ADDEND)?
    / MONTH_DIVISOR;
  let day = day_of_year.checked_sub(month_start)?.checked_add(FIRST)?;
  let month = if month_from_march < JANUARY_FROM_MARCH {
    month_from_march.checked_add(MARCH)?
  } else {
    month_from_march.checked_sub(MARCH_SHIFT)?
  };
  Some((month, day))
}

/// Seconds since midnight of a time of day, `None` for a field past its range.
pub(crate) fn seconds_of_day(hour: u64, minute: u64, second: u64) -> Option<u64> {
  if hour >= HOURS_PER_DAY || minute >= SIXTY || second >= SIXTY {
    return None;
  }
  hour
    .checked_mul(SECONDS_PER_HOUR)?
    .checked_add(minute.checked_mul(SIXTY)?)?
    .checked_add(second)
}

/// The hour, minute and second of a count of seconds since midnight.
pub(crate) fn time_of_day(seconds: u64) -> (u64, u64, u64) {
  (
    seconds / SECONDS_PER_HOUR,
    (seconds / SIXTY) % SIXTY,
    seconds % SIXTY,
  )
}

/// The days in `month` of `year`.
fn days_in_month(year: u64, month: u64) -> Option<u64> {
  let common = MONTH_LENGTHS.get(usize::try_from(month.checked_sub(FIRST)?).ok()?)?;
  let leap = year.is_multiple_of(LEAP_EVERY)
    && (!year.is_multiple_of(CENTURY) || year.is_multiple_of(ERA_YEARS));
  Some(if month == FEBRUARY && leap {
    common.checked_add(FIRST)?
  } else {
    *common
  })
}
