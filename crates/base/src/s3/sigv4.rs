//! AWS Signature Version 4 for S3 requests (A-464; §4.15 "An S3 base"). A request is signed by its canonical form: the
//! method, the URI-encoded path, the canonical query string, the canonical headers and the names signed, and the
//! payload's hash; then a string to sign naming the time, the scope and that form's SHA-256; then an HMAC-SHA256 chain
//! from the secret to a key for the day, the region and the service, and the signature over the string to sign.
//!
//! Every rule here is AWS's, from two pages fetched 2026-11-18:
//! - S3's "Signature Calculations for the Authorization Header: Transferring Payload in a Single Chunk"
//!   (`docs.aws.amazon.com/AmazonS3/latest/developerguide/sig-v4-header-based-auth.html`): the canonical request's six
//!   lines, `UriEncode` (every byte but `A-Z a-z 0-9 - . _ ~` as `%XX` in upper case, a space as `%20`, `/` kept in
//!   an object key's path and encoded everywhere else), a path never normalized and encoded once, the query's names
//!   and values encoded and then sorted, the scope `YYYYMMDD/region/s3/aws4_request`, and the key chain. Its four
//!   worked examples (GET Object, PUT Object, GET Bucket Lifecycle, List Objects) are this module's golden vectors.
//! - IAM's "Create a signed AWS API request" (`reference_sigv-create-signed-request`): a header's value trimmed, its
//!   sequential spaces made one, a repeated header's values joined by commas, and a temporary credential's
//!   `x-amz-security-token` signed with the rest.
//!
//! The secret and the session token never leave a [`Credentials`] but as the signing key's first input and the token's
//! header: its `Debug` names neither, so no log line can carry one (§4.15 "An S3 base": credentials are Mantle's, held
//! in RAM, never logged). SHA-256 and HMAC-SHA256 are aws-lc-rs's, the workspace's one cryptographic library.

use std::fmt;

use aws_lc_rs::{digest, hmac};

/// Format: the signing algorithm's name, which opens a string to sign and an `Authorization` header.
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
/// Format: the service name S3's scope carries.
pub const SERVICE: &str = "s3";
/// Format: the word that closes every scope.
const SCOPE_END: &str = "aws4_request";
/// Format: the prefix of the first key in the chain, before the secret.
const KEY_PREFIX: &str = "AWS4";
/// Format: the hex SHA-256 of an empty payload, which a GET or a HEAD signs (S3's page, "HashedPayload").
pub const EMPTY_PAYLOAD_SHA256: &str =
  "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// Format: the header a temporary credential's session token rides in, signed with the rest.
pub const SESSION_TOKEN_HEADER: &str = "x-amz-security-token";
/// Format: a timestamp's length in ISO 8601 basic form, `YYYYMMDD'T'HHMMSS'Z'`.
const TIMESTAMP_LEN: usize = 16;
/// Format: a date's length, `YYYYMMDD`, the timestamp's first part and the scope's first field.
const DATE_LEN: usize = 8;

/// Format: seconds in a day, the timestamp's split of a Unix time into its day and its time of day.
const SECONDS_PER_DAY: u64 = 86_400;
/// Format: seconds in an hour and in a minute.
const SECONDS_PER_HOUR: u64 = 3_600;
/// Format: see [`SECONDS_PER_HOUR`].
const SECONDS_PER_MINUTE: u64 = 60;
/// Format: the days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar, which shifts the Unix epoch's
/// day count into an era that begins on a March 1st (Howard Hinnant, "chrono-Compatible Low-Level Date Algorithms",
/// `civil_from_days`).
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

/// `unix_seconds` as `x-amz-date`'s timestamp, `YYYYMMDD'T'HHMMSS'Z'` in UTC: the day by Hinnant's `civil_from_days`
/// over the proleptic Gregorian calendar, with years counted from March so that a leap day ends its year.
pub fn amz_timestamp(unix_seconds: u64) -> String {
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
  let year = year_of_era
    .saturating_add(era.saturating_mul(YEARS_PER_ERA))
    .saturating_add(u64::from(month < MARCH));
  let hour = time.checked_div(SECONDS_PER_HOUR).unwrap_or(0);
  let minute = time
    .checked_rem(SECONDS_PER_HOUR)
    .and_then(|rest| rest.checked_div(SECONDS_PER_MINUTE))
    .unwrap_or(0);
  let second = time.checked_rem(SECONDS_PER_MINUTE).unwrap_or(0);
  format!("{year:04}{month:02}{day_of_month:02}T{hour:02}{minute:02}{second:02}Z")
}

/// A credential to sign with, as Mantle hands it: an access key id, its secret, and, for a temporary credential, its
/// session token. Its `Debug` shows the access key id alone.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
  access_key_id: String,
  secret_access_key: String,
  session_token: Option<String>,
}

impl Credentials {
  /// A credential of `access_key_id` and its `secret_access_key`, with `session_token` for a temporary one.
  pub fn new(
    access_key_id: &str,
    secret_access_key: &str,
    session_token: Option<&str>,
  ) -> Credentials {
    Credentials {
      access_key_id: access_key_id.to_owned(),
      secret_access_key: secret_access_key.to_owned(),
      session_token: session_token.map(str::to_owned),
    }
  }

  /// The access key id, which a signature's credential names in the clear.
  pub fn access_key_id(&self) -> &str {
    &self.access_key_id
  }

  /// The session token a temporary credential sends in [`SESSION_TOKEN_HEADER`], if it has one.
  pub fn session_token(&self) -> Option<&str> {
    self.session_token.as_deref()
  }
}

impl fmt::Debug for Credentials {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Credentials")
      .field("access_key_id", &self.access_key_id)
      .field("secret_access_key", &"(withheld)")
      .field(
        "session_token",
        &self.session_token.as_ref().map(|_| "(withheld)"),
      )
      .finish()
  }
}

/// One request to sign: its method, its path (an object key's path, `/` and all, not yet encoded), its query parameters
/// (not yet encoded; an empty value for a subresource), the headers it sends (each one signed), and its payload's hex
/// SHA-256 ([`EMPTY_PAYLOAD_SHA256`] for none, or `UNSIGNED-PAYLOAD`).
#[derive(Clone, Copy, Debug)]
pub struct Request<'a> {
  /// The method, `GET` or `HEAD` for a base's reads.
  pub method: &'a str,
  /// The path, from its `/`, as the object's key names it.
  pub path: &'a str,
  /// The query's parameters, as names and values.
  pub query: &'a [(&'a str, &'a str)],
  /// The headers sent, every one signed: `host` and the `x-amz-*` ones at least.
  pub headers: &'a [(&'a str, &'a str)],
  /// The payload's hash as `x-amz-content-sha256` states it.
  pub payload_hash: &'a str,
}

/// When and where a request is signed: its timestamp, `YYYYMMDD'T'HHMMSS'Z'` in UTC, and its region.
#[derive(Clone, Copy, Debug)]
pub struct When<'a> {
  /// The timestamp `x-amz-date` carries.
  pub timestamp: &'a str,
  /// The region the bucket is in.
  pub region: &'a str,
}

/// A signing refused before a hash is taken: a malformed timestamp or region, which a scope could not carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignError {
  /// The timestamp is not `YYYYMMDD'T'HHMMSS'Z'`.
  Timestamp,
  /// The region is empty or holds a `/`, which would change the scope's fields.
  Region,
}

/// A signed request: its canonical form and string to sign (for a test or a refusal's diagnosis), the names signed,
/// the signature, and the `Authorization` header that carries them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signed {
  /// The canonical request.
  pub canonical_request: String,
  /// The string to sign.
  pub string_to_sign: String,
  /// The signed headers' names, sorted and joined by `;`.
  pub signed_headers: String,
  /// The signature, lower-case hex.
  pub signature: String,
  /// The `Authorization` header's value.
  pub authorization: String,
}

/// Whether `byte` is one `UriEncode` leaves as it is: `A-Z a-z 0-9 - . _ ~`.
fn unreserved(byte: u8) -> bool {
  byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// Format: the hex digits `UriEncode` writes, upper case.
const UPPER_HEX: &[u8; 16] = b"0123456789ABCDEF";
/// Format: the hex digits a hash and a signature are written in, lower case.
const LOWER_HEX: &[u8; 16] = b"0123456789abcdef";

/// `input` as `UriEncode` writes it: every byte but an unreserved one as `%XX` in upper case, a space as `%20`, and `/`
/// kept when `encode_slash` is false (an object key's path) and encoded otherwise (a query's names and values).
pub fn uri_encode(input: &str, encode_slash: bool) -> String {
  let mut encoded = String::with_capacity(input.len());
  for &byte in input.as_bytes() {
    if unreserved(byte) || (byte == b'/' && !encode_slash) {
      encoded.push(char::from(byte));
    } else {
      encoded.push('%');
      for nibble in [byte >> 4, byte & 0x0f] {
        if let Some(&digit) = UPPER_HEX.get(usize::from(nibble)) {
          encoded.push(char::from(digit));
        }
      }
    }
  }
  encoded
}

/// `bytes` in lower-case hex.
fn hex(bytes: &[u8]) -> String {
  let mut text = String::with_capacity(bytes.len().saturating_mul(2));
  for &byte in bytes {
    for nibble in [byte >> 4, byte & 0x0f] {
      if let Some(&digit) = LOWER_HEX.get(usize::from(nibble)) {
        text.push(char::from(digit));
      }
    }
  }
  text
}

/// The hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
  hex(digest::digest(&digest::SHA256, bytes).as_ref())
}

/// A header's value as the canonical headers carry it: trimmed, and each run of spaces made one.
fn canonical_value(value: &str) -> String {
  value
    .split(' ')
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

/// The canonical headers and the signed headers' names: each name in lower case, the headers sorted by it, a repeated
/// header's values joined by commas in the order sent.
fn canonical_headers(headers: &[(&str, &str)]) -> (String, String) {
  let mut by_name: std::collections::BTreeMap<String, Vec<String>> =
    std::collections::BTreeMap::new();
  for (name, value) in headers {
    by_name
      .entry(name.trim().to_ascii_lowercase())
      .or_default()
      .push(canonical_value(value));
  }
  let mut canonical = String::new();
  for (name, values) in &by_name {
    canonical.push_str(name);
    canonical.push(':');
    canonical.push_str(&values.join(","));
    canonical.push('\n');
  }
  let signed = by_name.keys().cloned().collect::<Vec<_>>().join(";");
  (canonical, signed)
}

/// The canonical query string: each name and value encoded, the pairs sorted by name and then value once encoded,
/// joined by `&`; an empty string for no query. A request's target carries the same string, so what is sent is what was
/// signed.
pub fn canonical_query(query: &[(&str, &str)]) -> String {
  let mut pairs: Vec<(String, String)> = query
    .iter()
    .map(|(name, value)| (uri_encode(name, true), uri_encode(value, true)))
    .collect();
  pairs.sort();
  pairs
    .iter()
    .map(|(name, value)| format!("{name}={value}"))
    .collect::<Vec<_>>()
    .join("&")
}

/// `request`'s canonical form and its signed headers' names: the method, the path encoded once (`/` kept, never
/// normalized; `/` for none), the canonical query, the canonical headers, the names signed and the payload's hash,
/// one a line.
pub fn canonical_request(request: &Request<'_>) -> (String, String) {
  let path = if request.path.is_empty() {
    "/".to_owned()
  } else {
    uri_encode(request.path, false)
  };
  let (headers, signed) = canonical_headers(request.headers);
  let canonical = format!(
    "{}\n{path}\n{}\n{headers}\n{signed}\n{}",
    request.method,
    canonical_query(request.query),
    request.payload_hash,
  );
  (canonical, signed)
}

/// The date part of a timestamp, once the timestamp is checked to be `YYYYMMDD'T'HHMMSS'Z'`.
fn date_of(timestamp: &str) -> Result<&str, SignError> {
  let bytes = timestamp.as_bytes();
  let shaped = bytes.len() == TIMESTAMP_LEN
    && bytes.iter().enumerate().all(|(at, byte)| match at {
      DATE_LEN => *byte == b'T',
      at if at == TIMESTAMP_LEN.saturating_sub(1) => *byte == b'Z',
      _ => byte.is_ascii_digit(),
    });
  if !shaped {
    return Err(SignError::Timestamp);
  }
  timestamp.get(..DATE_LEN).ok_or(SignError::Timestamp)
}

/// The scope a signature is bound to: `YYYYMMDD/region/s3/aws4_request`.
fn scope_of(when: &When<'_>) -> Result<String, SignError> {
  if when.region.is_empty() || when.region.contains('/') {
    return Err(SignError::Region);
  }
  Ok(format!(
    "{}/{}/{SERVICE}/{SCOPE_END}",
    date_of(when.timestamp)?,
    when.region
  ))
}

/// The string to sign for `canonical` at `when`: the algorithm, the timestamp, the scope, and the canonical request's
/// hex SHA-256, one a line.
pub fn string_to_sign(when: &When<'_>, canonical: &str) -> Result<String, SignError> {
  Ok(format!(
    "{ALGORITHM}\n{}\n{}\n{}",
    when.timestamp,
    scope_of(when)?,
    sha256_hex(canonical.as_bytes())
  ))
}

/// HMAC-SHA256 of `data` under `key`.
fn hmac_sha256(key: &[u8], data: &[u8]) -> hmac::Tag {
  hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, key), data)
}

/// The signing key for `secret` on the day `date` in `region`: the chain of HMAC-SHA256 from `"AWS4" + secret` through
/// the date, the region, the service and `aws4_request`.
fn signing_key(secret: &str, date: &str, region: &str) -> hmac::Tag {
  let first = format!("{KEY_PREFIX}{secret}");
  let day = hmac_sha256(first.as_bytes(), date.as_bytes());
  let place = hmac_sha256(day.as_ref(), region.as_bytes());
  let service = hmac_sha256(place.as_ref(), SERVICE.as_bytes());
  hmac_sha256(service.as_ref(), SCOPE_END.as_bytes())
}

/// Signs `request` with `credentials` at `when`: its canonical form, its string to sign, the signature (lower-case hex
/// HMAC-SHA256 of the string to sign under the day's signing key) and the `Authorization` header. The caller sends the
/// headers it signed, a temporary credential's session token among them ([`SESSION_TOKEN_HEADER`]).
pub fn sign(
  credentials: &Credentials,
  when: &When<'_>,
  request: &Request<'_>,
) -> Result<Signed, SignError> {
  let (canonical_request, signed_headers) = canonical_request(request);
  let string_to_sign = string_to_sign(when, &canonical_request)?;
  let key = signing_key(
    &credentials.secret_access_key,
    date_of(when.timestamp)?,
    when.region,
  );
  let signature = hex(hmac_sha256(key.as_ref(), string_to_sign.as_bytes()).as_ref());
  let authorization = format!(
    "{ALGORITHM} Credential={}/{},SignedHeaders={signed_headers},Signature={signature}",
    credentials.access_key_id,
    scope_of(when)?,
  );
  Ok(Signed {
    canonical_request,
    string_to_sign,
    signed_headers,
    signature,
    authorization,
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Format: the example credential of S3's signing page.
  const ACCESS_KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
  /// Format: see [`ACCESS_KEY_ID`].
  const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
  /// Format: the examples' time and region.
  const WHEN: When<'static> = When {
    timestamp: "20130524T000000Z",
    region: "us-east-1",
  };
  /// Format: the examples' host, the bucket virtual-hosted.
  const HOST: &str = "examplebucket.s3.amazonaws.com";
  /// Format: the PUT example's payload hash, the SHA-256 of `Welcome to Amazon S3.`.
  const PUT_PAYLOAD: &str = "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072";

  /// One worked example of S3's signing page: the request, and the canonical request, string to sign, signature and
  /// `Authorization` header the page gives for it.
  struct Example {
    request: Request<'static>,
    canonical_request: &'static str,
    string_to_sign: &'static str,
    signature: &'static str,
    authorization: &'static str,
  }

  /// The page's four examples, verbatim (fetched 2026-11-18).
  fn examples() -> [Example; 4] {
    [
      Example {
        request: Request {
          method: "GET",
          path: "/test.txt",
          query: &[],
          headers: &[
            ("Host", HOST),
            ("Range", "bytes=0-9"),
            ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256),
            ("x-amz-date", "20130524T000000Z"),
          ],
          payload_hash: EMPTY_PAYLOAD_SHA256,
        },
        canonical_request: "GET\n/test.txt\n\nhost:examplebucket.s3.amazonaws.com\nrange:bytes=0-9\n\
                            x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
                            x-amz-date:20130524T000000Z\n\nhost;range;x-amz-content-sha256;x-amz-date\n\
                            e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        string_to_sign: "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n\
                         7344ae5b7ee6c3e7e6b0fe0640412a37625d1fbfff95c48bbb2dc43964946972",
        signature: "f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
        authorization: "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
                        SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
                        Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41",
      },
      Example {
        request: Request {
          method: "PUT",
          path: "/test$file.text",
          query: &[],
          headers: &[
            ("Host", HOST),
            ("Date", "Fri, 24 May 2013 00:00:00 GMT"),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
            ("x-amz-content-sha256", PUT_PAYLOAD),
          ],
          payload_hash: PUT_PAYLOAD,
        },
        canonical_request: "PUT\n/test%24file.text\n\ndate:Fri, 24 May 2013 00:00:00 GMT\n\
                            host:examplebucket.s3.amazonaws.com\n\
                            x-amz-content-sha256:44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072\n\
                            x-amz-date:20130524T000000Z\nx-amz-storage-class:REDUCED_REDUNDANCY\n\n\
                            date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class\n\
                            44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072",
        string_to_sign: "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n\
                         9e0e90d9c76de8fa5b200d8c849cd5b8dc7a3be3951ddb7f6a76b4158342019d",
        signature: "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd",
        authorization: "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
                        SignedHeaders=date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class,\
                        Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd",
      },
      Example {
        request: Request {
          method: "GET",
          path: "/",
          query: &[("lifecycle", "")],
          headers: &[
            ("Host", HOST),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256),
          ],
          payload_hash: EMPTY_PAYLOAD_SHA256,
        },
        canonical_request: "GET\n/\nlifecycle=\nhost:examplebucket.s3.amazonaws.com\n\
                            x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
                            x-amz-date:20130524T000000Z\n\nhost;x-amz-content-sha256;x-amz-date\n\
                            e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        string_to_sign: "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n\
                         9766c798316ff2757b517bc739a67f6213b4ab36dd5da2f94eaebf79c77395ca",
        signature: "fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543",
        authorization: "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
                        SignedHeaders=host;x-amz-content-sha256;x-amz-date,\
                        Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543",
      },
      Example {
        request: Request {
          method: "GET",
          path: "/",
          query: &[("max-keys", "2"), ("prefix", "J")],
          headers: &[
            ("Host", HOST),
            ("x-amz-date", "20130524T000000Z"),
            ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256),
          ],
          payload_hash: EMPTY_PAYLOAD_SHA256,
        },
        canonical_request: "GET\n/\nmax-keys=2&prefix=J\nhost:examplebucket.s3.amazonaws.com\n\
                            x-amz-content-sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855\n\
                            x-amz-date:20130524T000000Z\n\nhost;x-amz-content-sha256;x-amz-date\n\
                            e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        string_to_sign: "AWS4-HMAC-SHA256\n20130524T000000Z\n20130524/us-east-1/s3/aws4_request\n\
                         df57d21db20da04d7fa30298dd4488ba3a2b47ca3a489c74750e0f1e7df1b9b7",
        signature: "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7",
        authorization: "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
                        SignedHeaders=host;x-amz-content-sha256;x-amz-date,\
                        Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7",
      },
    ]
  }

  /// A-464 (golden vectors): do sign each of the four worked examples of S3's signing page (GET Object with a range,
  /// PUT Object with a `$` in its key, GET Bucket Lifecycle's subresource, List Objects' sorted query) with the page's
  /// credential and time. Expect the canonical request, the string to sign, the signature and the `Authorization`
  /// header the page gives, character for character.
  #[test]
  fn the_s3_signing_pages_four_examples_sign_as_the_page_gives() {
    let credentials = Credentials::new(ACCESS_KEY_ID, SECRET, None);
    for example in examples() {
      let signed = sign(&credentials, &WHEN, &example.request).unwrap();
      assert_eq!(signed.canonical_request, example.canonical_request);
      assert_eq!(signed.string_to_sign, example.string_to_sign);
      assert_eq!(signed.signature, example.signature);
      assert_eq!(signed.authorization, example.authorization);
    }
  }

  /// A-464 (IAM's header rules): do sign a request whose headers come in any case and order, with spaces around and
  /// within a value and a header sent twice. Expect the names lower-cased and sorted, the value trimmed with each run
  /// of spaces made one, the repeated header's values joined by a comma in the order sent, and the same signature as
  /// the request sent tidy.
  #[test]
  fn headers_are_canonical_whatever_their_case_order_and_spacing() {
    let credentials = Credentials::new(ACCESS_KEY_ID, SECRET, None);
    let messy = Request {
      method: "GET",
      path: "/a key",
      query: &[],
      headers: &[
        ("X-Amz-Date", "20130524T000000Z"),
        ("x-amz-meta-two", "  one   two  "),
        ("HOST", HOST),
        ("x-amz-meta-two", "three"),
      ],
      payload_hash: EMPTY_PAYLOAD_SHA256,
    };
    let tidy = Request {
      headers: &[
        ("host", HOST),
        ("x-amz-date", "20130524T000000Z"),
        ("x-amz-meta-two", "one two,three"),
      ],
      ..messy
    };
    let signed = sign(&credentials, &WHEN, &messy).unwrap();
    assert_eq!(
      signed.canonical_request,
      "GET\n/a%20key\n\nhost:examplebucket.s3.amazonaws.com\nx-amz-date:20130524T000000Z\n\
       x-amz-meta-two:one two,three\n\nhost;x-amz-date;x-amz-meta-two\n\
       e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(signed, sign(&credentials, &WHEN, &tidy).unwrap());
  }

  /// A-464 (`UriEncode`): do encode a key's path and a query's value holding every class of byte. Expect the unreserved
  /// bytes kept, a space `%20` (never `+`), `/` kept in a path and `%2F` in a query, and every other byte of the UTF-8
  /// encoding `%XX` in upper case; and a query sorted by its encoded names.
  #[test]
  fn uri_encoding_keeps_the_unreserved_and_encodes_the_rest() {
    assert_eq!(
      uri_encode("AZaz09-._~ /é+%", false),
      "AZaz09-._~%20/%C3%A9%2B%25"
    );
    assert_eq!(uri_encode("a/b c", true), "a%2Fb%20c");
    assert_eq!(
      canonical_query(&[
        ("prefix", "a b/"),
        ("list-type", "2"),
        ("continuation-token", "x+y=")
      ]),
      "continuation-token=x%2By%3D&list-type=2&prefix=a%20b%2F"
    );
  }

  /// A-464 (temporary credentials): do sign with a session token, sending it in `x-amz-security-token`. Expect the
  /// token signed with the rest, and the credential's `Debug` naming the access key id and never the secret or the
  /// token.
  #[test]
  fn a_session_token_is_signed_and_no_secret_is_ever_shown() {
    let token = "FwoGZXIvYXdzEXAMPLETOKEN";
    let credentials = Credentials::new(ACCESS_KEY_ID, SECRET, Some(token));
    let headers = [
      ("host", HOST),
      ("x-amz-content-sha256", EMPTY_PAYLOAD_SHA256),
      ("x-amz-date", "20130524T000000Z"),
      (SESSION_TOKEN_HEADER, credentials.session_token().unwrap()),
    ];
    let request = Request {
      method: "HEAD",
      path: "/test.txt",
      query: &[],
      headers: &headers,
      payload_hash: EMPTY_PAYLOAD_SHA256,
    };
    let signed = sign(&credentials, &WHEN, &request).unwrap();
    assert!(
      signed
        .signed_headers
        .ends_with(";x-amz-date;x-amz-security-token")
    );
    let shown = format!("{credentials:?}");
    assert!(shown.contains(ACCESS_KEY_ID));
    assert!(!shown.contains(SECRET) && !shown.contains(token), "{shown}");
  }

  /// A-471: do format Unix times as `x-amz-date` timestamps: the epoch, the S3 page's example time, a leap day in a
  /// century leap year and in an ordinary one, and the last second of year 9999. Expect each as the calendar has it.
  #[test]
  fn unix_times_format_as_the_calendar_has_them() {
    for (seconds, timestamp) in [
      (0, "19700101T000000Z"),
      (1_369_353_600, "20130524T000000Z"),
      (951_782_400, "20000229T000000Z"),
      (1_709_164_800, "20240229T000000Z"),
      (253_402_300_799, "99991231T235959Z"),
    ] {
      assert_eq!(amz_timestamp(seconds), timestamp, "{seconds}");
    }
  }

  /// A-464: do sign at a timestamp not in `YYYYMMDD'T'HHMMSS'Z'` form and in a region a scope cannot carry. Expect each
  /// refused by its kind before anything is hashed.
  #[test]
  fn a_malformed_timestamp_or_region_is_refused() {
    let credentials = Credentials::new(ACCESS_KEY_ID, SECRET, None);
    let request = examples()[0].request;
    for timestamp in [
      "",
      "20130524",
      "20130524T000000",
      "2013-05-24T000000Z",
      "20130524X000000Z",
    ] {
      let when = When { timestamp, ..WHEN };
      assert_eq!(
        sign(&credentials, &when, &request),
        Err(SignError::Timestamp),
        "{timestamp}"
      );
    }
    for region in ["", "us/east"] {
      let when = When { region, ..WHEN };
      assert_eq!(sign(&credentials, &when, &request), Err(SignError::Region));
    }
  }
}
