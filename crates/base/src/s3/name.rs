//! An S3 base as the library names it (A-524; §4.15 "An S3 base"; Mantle's CP-T10.5): the store's endpoint, the
//! bucket's region, the bucket, the prefix the base's keys lie under, and how requests address the bucket. A name is
//! checked whole when it is made, so a base no request could read is refused by its kind before a volume is made over
//! it, never at its first read:
//! - the endpoint is `https://host[:port]`, or `http://` to a loopback host alone (a test's store: A-471's client
//!   refuses plain HTTP anywhere else); a host is a DNS name or an IPv4 address, with no user, path, query or fragment;
//! - the region is a DNS label of lowercase letters, digits and hyphens (RFC 1035 §2.3.4's 63 characters at most), as
//!   AWS's region codes are and as an endpoint name carries one (`s3.us-east-1.amazonaws.com`), so SigV4's scope can
//!   hold it;
//! - the bucket is as S3's general purpose bucket naming rules allow (S3's user guide, "General purpose bucket naming
//!   rules", fetched 2026-11-19): 3 to 63 characters of lowercase letters, digits, periods and hyphens, beginning and
//!   ending with a letter or a digit, no two adjacent periods, not formatted as an IP address. Its reserved prefixes
//!   and suffixes are not refused: they name access point aliases, Multi-Region Access Points and directory buckets,
//!   which a read addresses as it addresses a bucket;
//! - a bucket with a period cannot be addressed virtual-hosted over HTTPS ("The security certificates used for virtual
//!   hosting of buckets don't work for buckets with periods in their names", the same guide), so such a base names
//!   path-style addressing or is refused;
//! - the prefix is empty or a key prefix ending in `/` whose every component a path can hold (the keys' own rules,
//!   [`super::keys`]), at most S3's key length.
//!
//! A base's identity ([`S3Base::identity`]) is a digest of what names its objects: the endpoint's host and port, the
//! bucket and the prefix. It is the device of every fingerprint the base's host reports, so two bases never share one
//! and a base names the same device across a restart. The region and the addressing style change how a request is
//! made, never which objects it reads, so they are not in it. A name holds no credential: those are handed apart and
//! held in RAM alone (§4.15 "Credentials").

use std::fmt;

use aws_lc_rs::digest;
use rustls::pki_types::CertificateDer;
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4};

use super::client::{Carriage, Endpoint, HTTPS_PORT, Style};
use super::keys::{self, SEPARATOR};
use super::list::KEY_MAX_BYTES;

/// Format: the port a plain `http` endpoint is served on when the name gives none (RFC 9110 §4.2.1).
pub const HTTP_PORT: u16 = 80;
/// Format: S3's bucket name length, 3 to 63 characters (the general purpose bucket naming rules).
const BUCKET_LENGTH: (usize, usize) = (3, 63);
/// Format: the longest DNS label (RFC 1035 §2.3.4: "labels 63 octets or less"), a region's longest.
const LABEL_MAX: usize = 63;
/// Format: an IPv4 address's dotted fields, which a bucket name must not be formatted as.
const IPV4_FIELDS: usize = 4;
/// Format: the domain that names what a base's identity digest is of, so it collides with no other digest.
const IDENTITY_DOMAIN: &[u8] = b"slates s3 base identity\0";
/// Format: the bytes of a digest the identity keeps: a `u64`'s eight.
const IDENTITY_BYTES: usize = size_of::<u64>();

/// How the endpoint is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
  /// HTTPS: TLS, the store's certificate verified.
  Https,
  /// Plain HTTP, to a loopback host alone.
  HttpLoopback,
}

/// Why a name was refused, by its kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameRefused {
  /// The endpoint is not `https://host[:port]` or `http://` to a loopback host.
  Endpoint(EndpointFault),
  /// The region is not a DNS label of lowercase letters, digits and hyphens.
  Region,
  /// The bucket breaks S3's naming rules.
  Bucket(BucketFault),
  /// Virtual-hosted addressing over HTTPS was asked for a bucket with a period, which no certificate can name.
  DottedVirtualHost,
  /// The prefix is not empty or a key prefix ending in `/` whose components a path can hold.
  Prefix(PrefixFault),
}

/// What is wrong with an endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointFault {
  /// Neither `https://` nor `http://`.
  Scheme,
  /// A user, a path, a query or a fragment.
  Extra,
  /// No host, or one that is neither a DNS name nor an IPv4 address.
  Host,
  /// A port that is not a decimal from 1 to 65,535.
  Port,
  /// Plain HTTP to a host that is not loopback.
  PlainOffLoopback,
}

/// What is wrong with a bucket name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BucketFault {
  /// Shorter than 3 or longer than 63 characters.
  Length,
  /// A character other than a lowercase letter, a digit, a period or a hyphen.
  Character,
  /// Beginning or ending with a period or a hyphen.
  Edge,
  /// Two adjacent periods.
  AdjacentPeriods,
  /// Formatted as an IP address.
  IpAddress,
}

/// What is wrong with a prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefixFault {
  /// Not empty and not ending in `/`, so it would split a name.
  Unterminated,
  /// A component no path can hold, by the keys' own rule.
  Component(keys::Refused),
}

/// An S3 base: where its objects are, and how requests reach them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Base {
  scheme: Scheme,
  host: String,
  port: u16,
  region: String,
  bucket: String,
  prefix: String,
  style: Style,
}

impl S3Base {
  /// The base of the objects under `prefix` in `bucket`, in `region`, at the store `endpoint`, addressed `style`; each
  /// part checked as the module states, refused by its kind.
  pub fn new(
    endpoint: &str,
    region: &str,
    bucket: &str,
    prefix: &str,
    style: Style,
  ) -> Result<S3Base, NameRefused> {
    let (scheme, host, port) = endpoint_of(endpoint).map_err(NameRefused::Endpoint)?;
    check_region(region)?;
    check_bucket(bucket).map_err(NameRefused::Bucket)?;
    if style == Style::VirtualHosted && scheme == Scheme::Https && bucket.contains('.') {
      return Err(NameRefused::DottedVirtualHost);
    }
    check_prefix(prefix).map_err(NameRefused::Prefix)?;
    Ok(S3Base {
      scheme,
      host: host.to_owned(),
      port,
      region: region.to_owned(),
      bucket: bucket.to_owned(),
      prefix: prefix.to_owned(),
      style,
    })
  }

  /// How the endpoint is reached.
  pub fn scheme(&self) -> Scheme {
    self.scheme
  }

  /// The endpoint's host.
  pub fn host(&self) -> &str {
    &self.host
  }

  /// The endpoint's port.
  pub fn port(&self) -> u16 {
    self.port
  }

  /// The bucket's region.
  pub fn region(&self) -> &str {
    &self.region
  }

  /// The bucket.
  pub fn bucket(&self) -> &str {
    &self.bucket
  }

  /// The prefix the base's keys lie under: empty, or ending in `/`.
  pub fn prefix(&self) -> &str {
    &self.prefix
  }

  /// How requests address the bucket.
  pub fn style(&self) -> Style {
    self.style
  }

  /// The base's identity: a digest of the endpoint's host and port, the bucket and the prefix, never zero.
  pub fn identity(&self) -> u64 {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(IDENTITY_DOMAIN);
    let port = self.port.to_be_bytes();
    for part in [
      self.host.as_bytes(),
      port.as_slice(),
      self.bucket.as_bytes(),
      self.prefix.as_bytes(),
    ] {
      context.update(&u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
      context.update(part);
    }
    let digest = context.finish();
    let mut word = [0u8; IDENTITY_BYTES];
    for (to, from) in word.iter_mut().zip(digest.as_ref()) {
      *to = *from;
    }
    u64::from_be_bytes(word).max(1)
  }

  /// The `Host` header a request to the base carries: the bucket before the endpoint's host when virtual-hosted, and
  /// the port where it is not the scheme's own.
  pub fn host_header(&self) -> String {
    let default_port = match self.scheme {
      Scheme::Https => HTTPS_PORT,
      Scheme::HttpLoopback => HTTP_PORT,
    };
    let host = match self.style {
      Style::VirtualHosted => format!("{}.{}", self.bucket, self.host),
      Style::Path => self.host.clone(),
    };
    if self.port == default_port {
      host
    } else {
      format!("{host}:{}", self.port)
    }
  }

  /// The client's endpoint for this base at `addr`, the address its host resolved to (the caller resolves it: A-471),
  /// verifying the store's certificate against `authorities` over HTTPS.
  pub fn endpoint(
    &self,
    addr: SocketAddrV4,
    authorities: Vec<CertificateDer<'static>>,
  ) -> Endpoint {
    Endpoint {
      addr,
      host: self.host_header(),
      region: self.region.clone(),
      bucket: self.bucket.clone(),
      style: self.style,
      carriage: match self.scheme {
        Scheme::Https => Carriage::Tls { authorities },
        Scheme::HttpLoopback => Carriage::PlainLoopback,
      },
    }
  }
}

impl fmt::Display for S3Base {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let scheme = match self.scheme {
      Scheme::Https => "https",
      Scheme::HttpLoopback => "http",
    };
    write!(
      f,
      "s3://{}/{} at {scheme}://{}:{} in {}",
      self.bucket, self.prefix, self.host, self.port, self.region
    )
  }
}

/// The scheme, host and port of an endpoint: `https://host[:port]`, or `http://host[:port]` with a loopback host.
fn endpoint_of(endpoint: &str) -> Result<(Scheme, &str, u16), EndpointFault> {
  let (scheme, authority) = if let Some(rest) = endpoint.strip_prefix("https://") {
    (Scheme::Https, rest)
  } else if let Some(rest) = endpoint.strip_prefix("http://") {
    (Scheme::HttpLoopback, rest)
  } else {
    return Err(EndpointFault::Scheme);
  };
  // A trailing `/` is the root path, which says nothing; anything past it, a user or a query is refused.
  let authority = authority.strip_suffix('/').unwrap_or(authority);
  if authority.contains(['/', '@', '?', '#']) {
    return Err(EndpointFault::Extra);
  }
  let (host, port) = match authority.rsplit_once(':') {
    Some((host, port)) => (host, port_of(port)?),
    None => (
      authority,
      match scheme {
        Scheme::Https => HTTPS_PORT,
        Scheme::HttpLoopback => HTTP_PORT,
      },
    ),
  };
  if !is_dns_name(host) && host.parse::<Ipv4Addr>().is_err() {
    return Err(EndpointFault::Host);
  }
  if scheme == Scheme::HttpLoopback && !is_loopback(host) {
    return Err(EndpointFault::PlainOffLoopback);
  }
  Ok((scheme, host, port))
}

/// A port: a decimal from 1 to 65,535.
fn port_of(port: &str) -> Result<u16, EndpointFault> {
  if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
    return Err(EndpointFault::Port);
  }
  match port.parse::<u16>() {
    Ok(0) | Err(_) => Err(EndpointFault::Port),
    Ok(port) => Ok(port),
  }
}

/// Whether `host` is a loopback host: an IPv4 address in 127.0.0.0/8, or `localhost`, which resolves to loopback alone
/// (RFC 6761 §6.3).
fn is_loopback(host: &str) -> bool {
  host.eq_ignore_ascii_case("localhost")
    || host
      .parse::<Ipv4Addr>()
      .is_ok_and(|address| address.is_loopback())
}

/// Whether `host` is a DNS name: labels of letters, digits and hyphens, each 1 to 63 characters, not beginning or
/// ending with a hyphen (RFC 1123 §2.1), joined by periods; a name all of digits and periods is an address, not a name.
fn is_dns_name(host: &str) -> bool {
  !host.is_empty()
    && !host.bytes().all(|b| b.is_ascii_digit() || b == b'.')
    && host.split('.').all(|label| {
      !label.is_empty()
        && label.len() <= LABEL_MAX
        && label
          .bytes()
          .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
    })
}

/// A region: a DNS label of lowercase letters, digits and hyphens.
fn check_region(region: &str) -> Result<(), NameRefused> {
  let label = !region.is_empty()
    && region.len() <= LABEL_MAX
    && region
      .bytes()
      .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
  if label {
    Ok(())
  } else {
    Err(NameRefused::Region)
  }
}

/// A bucket name by S3's general purpose bucket naming rules.
fn check_bucket(bucket: &str) -> Result<(), BucketFault> {
  if bucket.len() < BUCKET_LENGTH.0 || bucket.len() > BUCKET_LENGTH.1 {
    return Err(BucketFault::Length);
  }
  if !bucket
    .bytes()
    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
  {
    return Err(BucketFault::Character);
  }
  let edge = |b: Option<u8>| b.is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
  if !edge(bucket.bytes().next()) || !edge(bucket.bytes().last()) {
    return Err(BucketFault::Edge);
  }
  if bucket.contains("..") {
    return Err(BucketFault::AdjacentPeriods);
  }
  let fields: Vec<&str> = bucket.split('.').collect();
  if fields.len() == IPV4_FIELDS
    && fields
      .iter()
      .all(|field| field.bytes().all(|b| b.is_ascii_digit()))
  {
    return Err(BucketFault::IpAddress);
  }
  Ok(())
}

/// A prefix: empty, or a key prefix ending in `/` whose every component a path can hold, at most S3's key length.
fn check_prefix(prefix: &str) -> Result<(), PrefixFault> {
  if prefix.is_empty() {
    return Ok(());
  }
  if u64::try_from(prefix.len()).unwrap_or(u64::MAX) > KEY_MAX_BYTES {
    return Err(PrefixFault::Component(keys::Refused::KeyTooLong));
  }
  let inner = prefix
    .strip_suffix(SEPARATOR)
    .ok_or(PrefixFault::Unterminated)?;
  inner
    .split(SEPARATOR)
    .try_for_each(keys::check_name)
    .map_err(PrefixFault::Component)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The base of `bucket` at AWS's endpoint, path-style, with no prefix.
  fn at_aws(bucket: &str) -> Result<S3Base, NameRefused> {
    S3Base::new(
      "https://s3.us-east-1.amazonaws.com",
      "us-east-1",
      bucket,
      "",
      Style::Path,
    )
  }

  /// A-524: do name bases as Mantle would: a bucket at AWS's endpoint virtual-hosted with a prefix, a store on loopback
  /// over plain HTTP at its own port, path-style. Expect each named, its `Host` header as S3 wants it (the bucket
  /// before the host when virtual-hosted, the port where it is not the scheme's), and its client endpoint carried as
  /// named.
  #[test]
  fn a_base_is_named_with_its_host_header_and_its_clients_endpoint() {
    let aws = S3Base::new(
      "https://s3.us-east-1.amazonaws.com",
      "us-east-1",
      "kiln-bytes",
      "projects/x/",
      Style::VirtualHosted,
    )
    .unwrap();
    assert_eq!(aws.host_header(), "kiln-bytes.s3.us-east-1.amazonaws.com");
    assert_eq!((aws.port(), aws.prefix()), (HTTPS_PORT, "projects/x/"));
    let local = S3Base::new(
      "http://127.0.0.1:9000/",
      "garage",
      "store.test",
      "",
      Style::Path,
    )
    .unwrap();
    assert_eq!(local.host_header(), "127.0.0.1:9000");
    let endpoint = local.endpoint(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 9000), Vec::new());
    assert_eq!(endpoint.carriage, Carriage::PlainLoopback);
    assert_eq!(
      (endpoint.host.as_str(), endpoint.bucket.as_str()),
      ("127.0.0.1:9000", "store.test")
    );
    assert_eq!(
      aws.to_string(),
      "s3://kiln-bytes/projects/x/ at https://s3.us-east-1.amazonaws.com:443 in us-east-1"
    );
  }

  /// A-524: do name the same objects twice, with another region and style, and then another bucket, prefix and port.
  /// Expect one identity for what names the same objects and another for each that does not, never zero.
  #[test]
  fn an_identity_names_the_objects_and_nothing_else() {
    let base = at_aws("kiln-bytes").unwrap();
    let restyled = S3Base::new(
      "https://s3.us-east-1.amazonaws.com",
      "eu-west-1",
      "kiln-bytes",
      "",
      Style::VirtualHosted,
    )
    .unwrap();
    assert_eq!(base.identity(), restyled.identity());
    let others = [
      at_aws("kiln-other").unwrap(),
      S3Base::new(
        "https://s3.us-east-1.amazonaws.com",
        "us-east-1",
        "kiln-bytes",
        "a/",
        Style::Path,
      )
      .unwrap(),
      S3Base::new(
        "https://s3.us-east-1.amazonaws.com:8443",
        "us-east-1",
        "kiln-bytes",
        "",
        Style::Path,
      )
      .unwrap(),
    ];
    for other in &others {
      assert_ne!(other.identity(), base.identity(), "{other}");
      assert_ne!(other.identity(), 0);
    }
  }

  /// A-524 (S3's bucket naming rules; hostile names): do name buckets too short and too long, with an uppercase letter
  /// or an underscore, beginning or ending with a hyphen or a period, with two adjacent periods, formatted as an IP
  /// address; and a dotted bucket virtual-hosted over HTTPS. Expect each refused by its kind; and an access point
  /// alias, a dotted bucket path-style, and a name of three characters named.
  #[test]
  fn a_bucket_s3_would_not_name_is_refused_by_its_kind() {
    for (bucket, fault) in [
      ("ab", BucketFault::Length),
      (&*"a".repeat(64), BucketFault::Length),
      ("Kiln-bytes", BucketFault::Character),
      ("kiln_bytes", BucketFault::Character),
      ("-kiln", BucketFault::Edge),
      ("kiln.", BucketFault::Edge),
      ("kiln..bytes", BucketFault::AdjacentPeriods),
      ("192.168.5.4", BucketFault::IpAddress),
    ] {
      assert_eq!(at_aws(bucket), Err(NameRefused::Bucket(fault)), "{bucket}");
    }
    assert_eq!(
      S3Base::new(
        "https://s3.amazonaws.com",
        "us-east-1",
        "www.example.com",
        "",
        Style::VirtualHosted
      ),
      Err(NameRefused::DottedVirtualHost)
    );
    for bucket in [
      "my-ap-hrzrlukc5m36ft7okagglf3gmwluquse1b-s3alias",
      "www.example.com",
      "abc",
    ] {
      assert!(at_aws(bucket).is_ok(), "{bucket}");
    }
  }

  /// A-524 (hostile names): do name endpoints of another scheme, with a path, a user or a query, with no host or a host
  /// that is no name, with a port of zero, past 65,535 or not a number, and plain HTTP off loopback; regions empty,
  /// uppercase or with a slash; prefixes unterminated, with an empty, a dot or a dot-dot component, or past S3's key
  /// length. Expect each refused by its kind, nothing named.
  #[test]
  fn an_endpoint_region_or_prefix_no_request_could_use_is_refused_by_its_kind() {
    for (endpoint, fault) in [
      ("ftp://s3.amazonaws.com", EndpointFault::Scheme),
      ("s3.amazonaws.com", EndpointFault::Scheme),
      ("https://s3.amazonaws.com/bucket", EndpointFault::Extra),
      ("https://user@s3.amazonaws.com", EndpointFault::Extra),
      ("https://s3.amazonaws.com?x=1", EndpointFault::Extra),
      ("https://", EndpointFault::Host),
      ("https://-bad-.example", EndpointFault::Host),
      ("https://s3.amazonaws.com:0", EndpointFault::Port),
      ("https://s3.amazonaws.com:65536", EndpointFault::Port),
      ("https://s3.amazonaws.com:https", EndpointFault::Port),
      ("http://s3.amazonaws.com", EndpointFault::PlainOffLoopback),
      ("http://10.0.0.1:9000", EndpointFault::PlainOffLoopback),
    ] {
      assert_eq!(
        S3Base::new(endpoint, "us-east-1", "kiln-bytes", "", Style::Path),
        Err(NameRefused::Endpoint(fault)),
        "{endpoint}"
      );
    }
    for region in ["", "US-EAST-1", "us/east"] {
      assert_eq!(
        S3Base::new(
          "https://s3.amazonaws.com",
          region,
          "kiln-bytes",
          "",
          Style::Path
        ),
        Err(NameRefused::Region),
        "{region}"
      );
    }
    let long = format!("{}/", "a/".repeat(600));
    for (prefix, fault) in [
      ("projects", PrefixFault::Unterminated),
      (
        "/projects/",
        PrefixFault::Component(keys::Refused::EmptyComponent),
      ),
      (
        "a//b/",
        PrefixFault::Component(keys::Refused::EmptyComponent),
      ),
      ("a/./", PrefixFault::Component(keys::Refused::DotComponent)),
      ("../", PrefixFault::Component(keys::Refused::DotComponent)),
      (
        long.as_str(),
        PrefixFault::Component(keys::Refused::KeyTooLong),
      ),
    ] {
      assert_eq!(
        S3Base::new(
          "https://s3.amazonaws.com",
          "us-east-1",
          "kiln-bytes",
          prefix,
          Style::Path
        ),
        Err(NameRefused::Prefix(fault)),
        "{prefix}"
      );
    }
    assert!(
      S3Base::new(
        "http://localhost:9000",
        "auto",
        "kiln-bytes",
        "a/b/",
        Style::Path
      )
      .is_ok()
    );
  }
}
