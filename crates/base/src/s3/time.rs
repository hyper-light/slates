//! The calendar an S3 base reads and writes times in (A-471, A-524; §4.15 "An S3 base"): a Unix time as a civil date
//! and back, by Howard Hinnant's `civil_from_days` and `days_from_civil` ("chrono-Compatible Low-Level Date
//! Algorithms") over the proleptic Gregorian calendar, with years counted from March so that a leap day ends its year.
//! A request's `x-amz-date` is written from it (`sigv4::amz_timestamp`), and an object's `LastModified` read with it:
//! ISO 8601 in UTC, `YYYY-MM-DDTHH:MM:SS.sssZ` as S3's `ListObjectsV2` response syntax gives it, the fraction optional.
//! A time S3 could not have written (before 1970, past year 9999, a month or a day the calendar has not, a second past
//! 59) is refused, never wrapped into another.

use slates_transport::pacer::NANOS_PER_SECOND;

/// Format: seconds in a day, the split of a Unix time into its day and its time of day.
pub const SECONDS_PER_DAY: u64 = 86_400;
/// Format: seconds in an hour and in a minute.
pub const SECONDS_PER_HOUR: u64 = 3_600;
/// Format: see [`SECONDS_PER_HOUR`].
pub const SECONDS_PER_MINUTE: u64 = 60;
/// Format: the days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar, which shifts the Unix epoch's
/// day count into an era that begins on a March 1st (Hinnant's `civil_from_days`).
const EPOCH_SHIFT_DAYS: u64 = 719_468;
/// Format: a common year's days.
const DAYS_PER_YEAR: u64 = 365;
/// Format: the Gregorian leap rule's periods: a leap day every fourth year, none in a century's last year, and one
/// again every fourth century.
const YEARS_PER_LEAP: u64 = 4;
/// Format: see [`YEARS_PER_LEAP`].
const YEARS_PER_CENTURY: u64 = 100;
/// Format: see [`YEARS_PER_LEAP`]; an era, after which the calendar's days repeat.
const YEARS_PER_ERA: u64 = 400;
/// Derived: the days of four common years, the span after which the year formula takes a leap day off (1,460).
const DAYS_PER_FOUR_COMMON_YEARS: u64 = DAYS_PER_YEAR * YEARS_PER_LEAP;
/// Derived: the days of a century with no leap day in its last year: a hundred common years and 24 leap days (36,524).
const DAYS_PER_CENTURY: u64 =
  DAYS_PER_YEAR * YEARS_PER_CENTURY + YEARS_PER_CENTURY / YEARS_PER_LEAP - 1;
/// Derived: the days of an era: four hundred common years, a leap day every fourth year, less the three centuries' last
/// years that are no leap years (146,097).
const DAYS_PER_ERA: u64 = DAYS_PER_YEAR * YEARS_PER_ERA + YEARS_PER_ERA / YEARS_PER_LEAP
  - YEARS_PER_ERA / YEARS_PER_CENTURY
  + 1;
/// Format: the days of March through July, 31 + 30 + 31 + 30 + 31: counted from March, the months' lengths repeat in
/// runs of five (August through December, then January and February cut short), so a day `doy` of the year from March
/// lies in month `(5·doy + 2) / 153` (Hinnant's `civil_from_days`).
const DAYS_PER_FIVE_MONTHS: u64 = 153;
/// Format: see [`DAYS_PER_FIVE_MONTHS`].
const FIVE_MONTHS: u64 = 5;
/// Format: March's number, the first month of a year counted from March, which ends with February and its leap day.
const MARCH: u64 = 3;
/// Format: a year's months.
const MONTHS_PER_YEAR: u64 = 12;
/// Format: February's number, the month whose length the leap rule sets.
const FEBRUARY: u64 = 2;
/// Format: February's days in a common year and in a leap year.
const FEBRUARY_DAYS: (u64, u64) = (28, 29);
/// Format: the first and last years a four-digit ISO 8601 year names after the Unix epoch, the span S3's times lie in.
const YEARS: (u64, u64) = (1970, 9999);
/// Format: the hours of a day and the minutes of an hour, each a time's field ceiling.
const HOURS_PER_DAY: u64 = 24;
/// Format: the digits of each field of an ISO 8601 extended date and time: the year's four, and two for every other.
const YEAR_DIGITS: usize = 4;
/// Format: see [`YEAR_DIGITS`].
const FIELD_DIGITS: usize = 2;
/// Format: the digits of a fraction of a second at the finest a nanosecond count holds.
const FRACTION_DIGITS: usize = 9;
/// Format: the radix of a time's decimal digits.
const DECIMAL: u64 = 10;
/// Format: the digits of the milliseconds S3 writes after a listing's seconds (`.000Z`).
const MILLI_DIGITS: usize = 3;
/// Derived: nanoseconds in a millisecond, a second's ten to the third.
const NANOS_PER_MILLI: u64 = NANOS_PER_SECOND / 1_000;

/// A civil date and time of day in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
  /// The year.
  pub year: u64,
  /// The month, 1 to 12.
  pub month: u64,
  /// The day of the month, from 1.
  pub day: u64,
  /// The hour, 0 to 23.
  pub hour: u64,
  /// The minute, 0 to 59.
  pub minute: u64,
  /// The second, 0 to 59.
  pub second: u64,
}

/// `unix_seconds` as a civil date and time in UTC (Hinnant's `civil_from_days`).
pub fn civil_of(unix_seconds: u64) -> Civil {
  let day = unix_seconds.checked_div(SECONDS_PER_DAY).unwrap_or(0);
  let time = unix_seconds.checked_rem(SECONDS_PER_DAY).unwrap_or(0);
  let shifted = day.saturating_add(EPOCH_SHIFT_DAYS);
  let era = shifted.checked_div(DAYS_PER_ERA).unwrap_or(0);
  let day_of_era = shifted.saturating_sub(era.saturating_mul(DAYS_PER_ERA));
  let year_of_era = day_of_era
    .saturating_sub(
      day_of_era
        .checked_div(DAYS_PER_FOUR_COMMON_YEARS)
        .unwrap_or(0),
    )
    .saturating_add(day_of_era.checked_div(DAYS_PER_CENTURY).unwrap_or(0))
    .saturating_sub(
      day_of_era
        .checked_div(DAYS_PER_ERA.saturating_sub(1))
        .unwrap_or(0),
    )
    .checked_div(DAYS_PER_YEAR)
    .unwrap_or(0);
  let day_of_year = day_of_era.saturating_sub(
    year_of_era
      .saturating_mul(DAYS_PER_YEAR)
      .saturating_add(year_of_era.checked_div(YEARS_PER_LEAP).unwrap_or(0))
      .saturating_sub(year_of_era.checked_div(YEARS_PER_CENTURY).unwrap_or(0)),
  );
  let month_from_march = day_of_year
    .saturating_mul(FIVE_MONTHS)
    .saturating_add(2)
    .checked_div(DAYS_PER_FIVE_MONTHS)
    .unwrap_or(0);
  let day_of_month = day_of_year
    .saturating_sub(
      month_from_march
        .saturating_mul(DAYS_PER_FIVE_MONTHS)
        .saturating_add(2)
        .checked_div(FIVE_MONTHS)
        .unwrap_or(0),
    )
    .saturating_add(1);
  let counted = month_from_march.saturating_add(MARCH);
  let month = if counted > MONTHS_PER_YEAR {
    counted.saturating_sub(MONTHS_PER_YEAR)
  } else {
    counted
  };
  Civil {
    year: year_of_era
      .saturating_add(era.saturating_mul(YEARS_PER_ERA))
      .saturating_add(u64::from(month < MARCH)),
    month,
    day: day_of_month,
    hour: time.checked_div(SECONDS_PER_HOUR).unwrap_or(0),
    minute: time
      .checked_rem(SECONDS_PER_HOUR)
      .and_then(|rest| rest.checked_div(SECONDS_PER_MINUTE))
      .unwrap_or(0),
    second: time.checked_rem(SECONDS_PER_MINUTE).unwrap_or(0),
  }
}

/// The Unix seconds of `civil`, or none for a date the calendar has not (a month past 12, a day past its month's last,
/// an hour, minute or second past its field) or one before 1970 (Hinnant's `days_from_civil`).
pub fn unix_of(civil: Civil) -> Option<u64> {
  if !on_the_calendar(civil) {
    return None;
  }
  days_of(civil.year, civil.month, civil.day)?
    .checked_mul(SECONDS_PER_DAY)?
    .checked_add(civil.hour.checked_mul(SECONDS_PER_HOUR)?)?
    .checked_add(civil.minute.checked_mul(SECONDS_PER_MINUTE)?)?
    .checked_add(civil.second)
}

/// Whether `civil` names a time the calendar has, from 1970: a month of the year, a day of that month, and an hour,
/// minute and second within their fields.
fn on_the_calendar(civil: Civil) -> bool {
  (1..=MONTHS_PER_YEAR).contains(&civil.month)
    && civil.day >= 1
    && civil.day <= days_in_month(civil.year, civil.month)
    && civil.hour < HOURS_PER_DAY
    && civil.minute < SECONDS_PER_MINUTE
    && civil.second < SECONDS_PER_MINUTE
    && civil.year >= YEARS.0
}

/// The days from the Unix epoch to `month`'s `day` of `year`, or none before the epoch (Hinnant's `days_from_civil`,
/// its year counted from March so that a leap day ends its year).
fn days_of(year: u64, month: u64, day: u64) -> Option<u64> {
  let year = year.checked_sub(u64::from(month < MARCH))?;
  let era = year.checked_div(YEARS_PER_ERA)?;
  let year_of_era = year.checked_sub(era.checked_mul(YEARS_PER_ERA)?)?;
  let day_of_era = year_of_era
    .checked_mul(DAYS_PER_YEAR)?
    .checked_add(year_of_era.checked_div(YEARS_PER_LEAP)?)?
    .checked_sub(year_of_era.checked_div(YEARS_PER_CENTURY)?)?
    .checked_add(day_of_year_from_march(month, day)?)?;
  era
    .checked_mul(DAYS_PER_ERA)?
    .checked_add(day_of_era)?
    .checked_sub(EPOCH_SHIFT_DAYS)
}

/// `month`'s `day` as the day of a year counted from March 1st, its first day 0 (Hinnant's `days_from_civil`: the
/// months from March repeat their lengths in runs of five, [`DAYS_PER_FIVE_MONTHS`]).
fn day_of_year_from_march(month: u64, day: u64) -> Option<u64> {
  let month_from_march = if month >= MARCH {
    month.checked_sub(MARCH)?
  } else {
    month.checked_add(MONTHS_PER_YEAR)?.checked_sub(MARCH)?
  };
  DAYS_PER_FIVE_MONTHS
    .checked_mul(month_from_march)?
    .checked_add(2)?
    .checked_div(FIVE_MONTHS)?
    .checked_add(day)?
    .checked_sub(1)
}

/// The days of `month` in `year`: February's 28, or 29 in a leap year; 30 for April, June, September and November; 31
/// for the rest (the Gregorian calendar).
fn days_in_month(year: u64, month: u64) -> u64 {
  if month == FEBRUARY {
    return if leap(year) {
      FEBRUARY_DAYS.1
    } else {
      FEBRUARY_DAYS.0
    };
  }
  // From March the lengths run 31, 30, 31, 30, 31 and repeat: a month's length is the difference of the five-month
  // formula's starts, so the calendar's own rule gives it, not a table.
  let from_march = if month >= MARCH {
    month.saturating_sub(MARCH)
  } else {
    month.saturating_add(MONTHS_PER_YEAR).saturating_sub(MARCH)
  };
  let start = |m: u64| {
    DAYS_PER_FIVE_MONTHS
      .saturating_mul(m)
      .saturating_add(2)
      .checked_div(FIVE_MONTHS)
      .unwrap_or(0)
  };
  start(from_march.saturating_add(1)).saturating_sub(start(from_march))
}

/// Whether `year` has a leap day: every fourth year, but not a century's last unless the fourth century's.
fn leap(year: u64) -> bool {
  year.checked_rem(YEARS_PER_LEAP) == Some(0)
    && (year.checked_rem(YEARS_PER_CENTURY) != Some(0)
      || year.checked_rem(YEARS_PER_ERA) == Some(0))
}

/// `ns`, nanoseconds since the Unix epoch, as S3 writes a listing's `LastModified`: `YYYY-MM-DDTHH:MM:SS.sssZ`, to the
/// millisecond. A time before the epoch is written as the epoch.
pub fn iso8601_of(ns: i64) -> String {
  let ns = u64::try_from(ns).unwrap_or(0);
  let civil = civil_of(ns.checked_div(NANOS_PER_SECOND).unwrap_or(0));
  let millis = ns
    .checked_rem(NANOS_PER_SECOND)
    .and_then(|rest| rest.checked_div(NANOS_PER_MILLI))
    .unwrap_or(0);
  format!(
    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{millis:0width$}Z",
    civil.year,
    civil.month,
    civil.day,
    civil.hour,
    civil.minute,
    civil.second,
    width = MILLI_DIGITS
  )
}

/// The nanoseconds since the Unix epoch an ISO 8601 UTC time names (`YYYY-MM-DDTHH:MM:SS[.fraction]Z`, as S3 writes a
/// listing's `LastModified`), or none for any other text: another form, a field out of its range, a date the calendar
/// has not, a year before 1970 or past 9999, a fraction past nanoseconds, or a time past what a signed count of
/// nanoseconds holds (2262-04-11T23:47:16.854775807Z).
pub fn ns_of_iso8601(text: &str) -> Option<i64> {
  let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
  let (year, month, day) = date_of(date)?;
  let (clock, fraction) = match time.split_once('.') {
    Some((clock, fraction)) => (clock, Some(fraction)),
    None => (time, None),
  };
  let (hour, minute, second) = clock_of(clock)?;
  let seconds = unix_of(Civil {
    year,
    month,
    day,
    hour,
    minute,
    second,
  })?;
  let nanos = match fraction {
    None => 0,
    Some(fraction) => nanos_of_fraction(fraction)?,
  };
  i64::try_from(seconds.checked_mul(NANOS_PER_SECOND)?.checked_add(nanos)?).ok()
}

/// The year, month and day of an ISO 8601 extended date (`YYYY-MM-DD`), each exactly its digits and the year at most
/// 9999; none for any other text.
fn date_of(date: &str) -> Option<(u64, u64, u64)> {
  let mut fields = date.split('-');
  let year = digits(fields.next()?, YEAR_DIGITS)?;
  let month = digits(fields.next()?, FIELD_DIGITS)?;
  let day = digits(fields.next()?, FIELD_DIGITS)?;
  (fields.next().is_none() && year <= YEARS.1).then_some((year, month, day))
}

/// The hour, minute and second of an ISO 8601 extended time of day (`HH:MM:SS`), each exactly two digits; none for any
/// other text.
fn clock_of(clock: &str) -> Option<(u64, u64, u64)> {
  let mut fields = clock.split(':');
  let hour = digits(fields.next()?, FIELD_DIGITS)?;
  let minute = digits(fields.next()?, FIELD_DIGITS)?;
  let second = digits(fields.next()?, FIELD_DIGITS)?;
  fields.next().is_none().then_some((hour, minute, second))
}

/// A field of exactly `width` decimal digits.
fn digits(field: &str, width: usize) -> Option<u64> {
  if field.len() != width || !field.bytes().all(|b| b.is_ascii_digit()) {
    return None;
  }
  field.parse().ok()
}

/// A fraction of a second's digits (one to nine) as nanoseconds.
fn nanos_of_fraction(fraction: &str) -> Option<u64> {
  if fraction.is_empty()
    || fraction.len() > FRACTION_DIGITS
    || !fraction.bytes().all(|b| b.is_ascii_digit())
  {
    return None;
  }
  let value: u64 = fraction.parse().ok()?;
  let missing = u32::try_from(FRACTION_DIGITS.checked_sub(fraction.len())?).ok()?;
  value.checked_mul(DECIMAL.checked_pow(missing)?)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-524: do read S3's listing times: the API reference's sample, one with no fraction, a leap day, and the last
  /// millisecond a signed count of nanoseconds holds. Expect each as the calendar has it, and written back as S3 writes
  /// it.
  #[test]
  fn listing_times_read_as_the_calendar_has_them_and_write_back() {
    for (text, seconds, millis) in [
      ("2009-10-12T17:50:30.000Z", 1_255_369_830_u64, 0_u64),
      ("2013-05-24T00:00:00Z", 1_369_353_600, 0),
      ("2024-02-29T12:34:56.789Z", 1_709_210_096, 789),
      ("2262-04-11T23:47:16.854Z", 9_223_372_036, 854),
    ] {
      let ns = ns_of_iso8601(text).unwrap();
      assert_eq!(
        ns,
        i64::try_from(seconds * NANOS_PER_SECOND + millis * NANOS_PER_MILLI).unwrap()
      );
      let written = iso8601_of(ns);
      assert_eq!(ns_of_iso8601(&written), Some(ns), "{text} -> {written}");
    }
    assert_eq!(
      ns_of_iso8601("2013-05-24T00:00:00.123456789Z"),
      Some(1_369_353_600_123_456_789)
    );
    assert_eq!(
      iso8601_of(1_255_369_830_000_000_000),
      "2009-10-12T17:50:30.000Z"
    );
  }

  /// A-524 (hostile input): do read times S3 could not have written: another form, a month 13, February 30th and the
  /// 29th of a common year and of a century that is no leap year, an hour 24, a second 60, a year before 1970 or of
  /// five digits, a time past what a signed count of nanoseconds holds, a fraction of ten digits or none, a missing
  /// `Z`. Expect each refused.
  #[test]
  fn a_time_s3_could_not_write_is_refused() {
    for text in [
      "",
      "2009-10-12 17:50:30Z",
      "2009-10-12T17:50:30",
      "2009-13-12T17:50:30Z",
      "2009-02-30T00:00:00Z",
      "2023-02-29T00:00:00Z",
      "2100-02-29T00:00:00Z",
      "2009-10-12T24:00:00Z",
      "2009-10-12T17:50:60Z",
      "1969-12-31T23:59:59Z",
      "12009-10-12T17:50:30Z",
      "2262-04-11T23:47:17Z",
      "9999-12-31T23:59:59Z",
      "2009-10-12T17:50:30.1234567890Z",
      "2009-10-12T17:50:30.Z",
      "2009-1-12T17:50:30Z",
      "+009-10-12T17:50:30Z",
    ] {
      assert_eq!(ns_of_iso8601(text), None, "{text}");
    }
    assert!(
      ns_of_iso8601("2000-02-29T00:00:00Z").is_some(),
      "a fourth century's leap day"
    );
  }

  /// A-524: do turn every seventh day from the epoch to year 9999 into its civil date and back. Expect the same day.
  #[test]
  fn civil_dates_round_trip_over_the_calendars_span() {
    let last_day = 2_932_896_u64;
    let mut day = 0;
    while day <= last_day {
      let seconds = day * SECONDS_PER_DAY + (day * 37) % SECONDS_PER_DAY;
      assert_eq!(unix_of(civil_of(seconds)), Some(seconds), "day {day}");
      day += 7;
    }
  }
}
