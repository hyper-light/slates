//! A text held as a revlog chunk in a process that installed no chunk decoder (goal 93, main's ruling (b)): refused
//! typed, never read as raw. Its own binary, so no other test installs a decoder in its process. The directory is a
//! real one in the build output (`CARGO_TARGET_TMPDIR`), never `/tmp` (R1).
#![cfg(unix)]
#![allow(
  clippy::unwrap_used,
  clippy::disallowed_methods,
  clippy::cast_possible_truncation
)]

use std::path::Path;

use slates_land::store_root::{StoreGrant, StoreRoot};
use slates_store::log::{Header, SectionKind, Tool};
use slates_store::staging::Staging;
use slates_store::store::{Contents, Sizing, Store, StoreError};
use slates_store::texts::{self, Form, Place, TextCache, TextEntry};

/// Goal 93, main's ruling (b). Do: land a text held as a chunk, with no decoder installed; read it. Expect: refused
/// `NoChunkDecoder`.
#[test]
fn a_chunked_text_with_no_decoder_installed_is_refused() {
  let dir =
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("chunk-undecoded-{}", std::process::id()));
  let _ = std::fs::remove_dir_all(&dir);
  std::fs::create_dir_all(&dir).unwrap();
  let dir = dir.canonicalize().unwrap();
  let grant = StoreGrant {
    repository: 1,
    target: StoreRoot::identify(&dir).unwrap(),
    issued_ns: 0,
  };
  let sizing = Sizing {
    memory: 8 << 20,
    disk: 1 << 30,
    extent_bytes: 128 << 10,
    fanout: 8,
  };
  let mut store = Store::open(StoreRoot::open(&dir, &grant).unwrap(), sizing).unwrap();
  let text = b"utext held as a chunk".to_vec();
  let identity = *blake3::hash(&text).as_bytes();
  let section = texts::encode(&[TextEntry {
    path: b"f".to_vec(),
    tool_id: Vec::new(),
    text: identity,
    len: text.len() as u64 - 1,
    base: None,
    place: Place {
      payload: *blake3::hash(&text).as_bytes(),
      offset: 0,
      len: text.len() as u64,
    },
    form: Form::Chunk,
  }]);
  let header = Header {
    tool: Tool::Mercurial,
    repository: 1,
    sequence: 1,
    judged_over: 0,
    grant: 1,
    work: 1,
    time_ns: 1,
    sections: 0,
  };
  let staging = Staging::new(1 << 20);
  let parts = [
    (SectionKind::Record, Contents::Bytes(b"heads")),
    (SectionKind::Payload, Contents::Bytes(&text)),
    (SectionKind::Texts, Contents::Bytes(&section)),
  ];
  store.land(header, &parts, &staging).unwrap();
  let mut cache = TextCache::new(1 << 20);
  let read = store.text_at(&staging, 1, &identity, 0, &mut cache);
  assert!(matches!(read, Err(StoreError::NoChunkDecoder)), "{read:?}");
  let _ = std::fs::remove_dir_all(&dir);
}
