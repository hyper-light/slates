//! Interoperability of slates' SecP384r1MLKEM1024 (`slates_transport::kx`) with an independent implementation (condition
//! 8): a TLS 1.3 handshake over TCP that offers that group alone, against OpenSSL 3.5's (`openssl s_server -groups
//! SecP384r1MLKEM1024` / `openssl s_client -groups SecP384r1MLKEM1024`). The handshake completes only if both sides lay
//! out the shares and the concatenated secret the same way (the Finished messages are keyed from that secret), so a
//! completed handshake in both directions is the proof that slates' layout is the draft's, not merely self-consistent.
//!
//! `cargo run --release -p slates-transport --example tls_interop -- client HOST:PORT` connects and prints the group;
//! `... -- server PORT` accepts one connection and prints the group. Certificates are not verified: the subject here is
//! the key exchange, and the harness talks to a throwaway OpenSSL peer.

// Interop harness, not shipped code: it talks plain TCP to an OpenSSL peer (the lint wall reserves `std::net` for the
// product's own sockets), and an unwrap is a failed run.
#![allow(
  clippy::disallowed_types,
  clippy::disallowed_methods,
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::io::{Read, Write};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

/// The provider offering SecP384r1MLKEM1024 alone, so a completed handshake can only have used it.
fn only_the_hybrid() -> Arc<rustls::crypto::CryptoProvider> {
  let mut provider = rustls::crypto::aws_lc_rs::default_provider();
  provider.kx_groups = vec![slates_transport::kx::SECP384R1_MLKEM1024];
  Arc::new(provider)
}

/// Accepts any server certificate: the subject of this harness is the key exchange.
#[derive(Debug)]
struct AnyServer(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AnyServer {
  fn verify_server_cert(
    &self,
    _: &CertificateDer<'_>,
    _: &[CertificateDer<'_>],
    _: &ServerName<'_>,
    _: &[u8],
    _: UnixTime,
  ) -> Result<ServerCertVerified, rustls::Error> {
    Ok(ServerCertVerified::assertion())
  }

  fn verify_tls12_signature(
    &self,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls12_signature(
      message,
      cert,
      dss,
      &self.0.signature_verification_algorithms,
    )
  }

  fn verify_tls13_signature(
    &self,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
  ) -> Result<HandshakeSignatureValid, rustls::Error> {
    rustls::crypto::verify_tls13_signature(
      message,
      cert,
      dss,
      &self.0.signature_verification_algorithms,
    )
  }

  fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
    self.0.signature_verification_algorithms.supported_schemes()
  }
}

fn client(address: &str) {
  let provider = only_the_hybrid();
  let config = rustls::ClientConfig::builder_with_provider(provider.clone())
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AnyServer(provider)))
    .with_no_client_auth();
  let mut connection =
    rustls::ClientConnection::new(Arc::new(config), ServerName::try_from("interop").unwrap())
      .unwrap();
  let mut socket = std::net::TcpStream::connect(address).unwrap();
  let mut stream = rustls::Stream::new(&mut connection, &mut socket);
  stream.write_all(b"slates interop\n").unwrap();
  stream.flush().unwrap();
  report(
    stream
      .conn
      .negotiated_key_exchange_group()
      .map(|group| group.name()),
  );
}

fn server(port: &str) {
  let key = rcgen::KeyPair::generate().unwrap();
  let certificate = rcgen::CertificateParams::new(vec!["interop".to_owned()])
    .unwrap()
    .self_signed(&key)
    .unwrap();
  let config = rustls::ServerConfig::builder_with_provider(only_the_hybrid())
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
      vec![certificate.der().clone()],
      PrivateKeyDer::try_from(key.serialize_der()).unwrap(),
    )
    .unwrap();
  let listener = std::net::TcpListener::bind(format!("0.0.0.0:{port}")).unwrap();
  println!("listening on {port}");
  let (mut socket, _) = listener.accept().unwrap();
  let mut connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
  let mut stream = rustls::Stream::new(&mut connection, &mut socket);
  let mut line = [0u8; 64];
  let read = stream.read(&mut line).unwrap_or(0);
  println!(
    "received {:?}",
    String::from_utf8_lossy(&line[..read]).trim()
  );
  report(
    stream
      .conn
      .negotiated_key_exchange_group()
      .map(|group| group.name()),
  );
}

fn report(group: Option<rustls::NamedGroup>) {
  let expected = rustls::NamedGroup::from(slates_transport::kx::SECP384R1_MLKEM1024_CODEPOINT);
  match group {
    Some(group) if group == expected => println!("negotiated SecP384r1MLKEM1024 (0x11ed)"),
    other => {
      println!("negotiated {other:?}, not SecP384r1MLKEM1024");
      std::process::exit(1);
    }
  }
}

fn main() {
  let args: Vec<String> = std::env::args().collect();
  match (args.get(1).map(String::as_str), args.get(2)) {
    (Some("client"), Some(address)) => client(address),
    (Some("server"), Some(port)) => server(port),
    _ => {
      eprintln!("usage: tls_interop client HOST:PORT | server PORT");
      std::process::exit(2);
    }
  }
}
