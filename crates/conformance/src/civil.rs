//! The civil date of a Unix time, for the `date` field of a record (Part 6: every recorded
//! run carries its date). The conversion is Howard Hinnant's `civil_from_days` (the algorithm
//! `std::chrono` adopted, evidence C), exact for every day of the proleptic Gregorian calendar;
//! it needs no table and no locale, so a record made on any host names the same day.

/// Format: seconds in a day.
const SECONDS_PER_DAY: i64 = 86_400;
/// Format: the era length in days of the proleptic Gregorian calendar (400 years).
const DAYS_PER_ERA: i64 = 146_097;
/// Format: the day shift from 1970-01-01 to 0000-03-01, the algorithm's epoch.
const DAYS_TO_MARCH_EPOCH: i64 = 719_468;
/// Format: years per era.
const YEARS_PER_ERA: i64 = 400;
/// Format: the constants of the day-of-year to month arithmetic (5 × month + 2, over 153 days
/// per five-month cycle).
const MONTH_CYCLE_DAYS: i64 = 153;
/// Format: the day-of-year to month numerator.
const MONTH_NUMERATOR: i64 = 5;
/// Format: the day-of-year to month offset.
const MONTH_OFFSET: i64 = 2;
/// Format: months in a year.
const MONTHS_PER_YEAR: i64 = 12;
/// Format: the month the algorithm's year starts at (March).
const MARCH: i64 = 3;
/// Format: days per four-year cycle inside an era, before the leap-day correction.
const DAYS_PER_FOUR_YEARS: i64 = 1_460;
/// Format: days per century inside an era, before the leap-day correction.
const DAYS_PER_CENTURY: i64 = 36_524;
/// Format: days in a common year.
const DAYS_PER_YEAR: i64 = 365;
/// Format: the era's leap-day count is one per four years less one per century (`/4`, `/100`).
const FOUR: i64 = 4;
/// Format: a century in years.
const CENTURY: i64 = 100;

/// A civil date: year, month (1–12), day (1–31).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CivilDate {
  /// The year.
  pub year: i64,
  /// The month, 1–12.
  pub month: u8,
  /// The day of the month, 1–31.
  pub day: u8,
}

impl CivilDate {
  /// The civil date of `unix_seconds` (UTC).
  pub fn from_unix(unix_seconds: i64) -> CivilDate {
    let days = unix_seconds.div_euclid(SECONDS_PER_DAY);
    let shifted = days + DAYS_TO_MARCH_EPOCH;
    let era = shifted.div_euclid(DAYS_PER_ERA);
    let day_of_era = shifted - era * DAYS_PER_ERA;
    let year_of_era = (day_of_era - day_of_era / DAYS_PER_FOUR_YEARS
      + day_of_era / DAYS_PER_CENTURY
      - day_of_era / (DAYS_PER_ERA - 1))
      / DAYS_PER_YEAR;
    let day_of_year =
      day_of_era - (DAYS_PER_YEAR * year_of_era + year_of_era / FOUR - year_of_era / CENTURY);
    let month_index = (MONTH_NUMERATOR * day_of_year + MONTH_OFFSET) / MONTH_CYCLE_DAYS;
    let day = day_of_year - (MONTH_CYCLE_DAYS * month_index + MONTH_OFFSET) / MONTH_NUMERATOR + 1;
    let month = if month_index < MONTHS_PER_YEAR - MARCH + 1 {
      month_index + MARCH
    } else {
      month_index + MARCH - MONTHS_PER_YEAR
    };
    let year = year_of_era + era * YEARS_PER_ERA + i64::from(month <= MONTH_OFFSET);
    CivilDate {
      year,
      month: u8::try_from(month).unwrap_or(0),
      day: u8::try_from(day).unwrap_or(0),
    }
  }

  /// `YYYY-MM-DD`.
  pub fn iso(&self) -> String {
    format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
  }
}

#[cfg(test)]
mod tests {
  use super::CivilDate;

  /// Known points of the calendar: the epoch, a leap day, a century boundary, the day before
  /// the epoch (negative time), and a recent date. Do: convert. Expect: the named dates.
  #[test]
  fn the_conversion_names_known_dates() {
    let cases: [(i64, &str); 6] = [
      (0, "1970-01-01"),
      (951_782_400, "2000-02-29"),
      (946_684_800, "2000-01-01"),
      (-86_400, "1969-12-31"),
      (1_757_808_000, "2025-09-14"),
      (1_789_344_000, "2026-09-14"),
    ];
    for (seconds, expected) in cases {
      assert_eq!(CivilDate::from_unix(seconds).iso(), expected, "{seconds}");
    }
  }

  /// A time inside a day names that day, not the next: the last second of 1999 is still 1999.
  #[test]
  fn seconds_inside_a_day_belong_to_it() {
    assert_eq!(CivilDate::from_unix(946_684_799).iso(), "1999-12-31");
  }
}
