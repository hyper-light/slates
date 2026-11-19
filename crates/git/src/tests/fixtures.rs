//! The golden vectors (A-582): what stock git 2.50.1 (Apple Git-155) said on its own wire to the requests
//! `fixtures/make-fixtures.sh` wrote, and the requests themselves, made once on the Mac Studio on 2026-11-19 (the
//! script's header names the command), compiled in, never read at run time.

/// protocol v2's capability advertisement, as `info/refs?service=git-upload-pack` answers it.
pub(crate) const ADVERTISEMENT: &[u8] = include_bytes!("../../fixtures/advertisement.bin");
/// The same, the server allowing `sideband-all` and serving a blob by `packfile-uris`.
pub(crate) const ADVERTISEMENT_URIS: &[u8] =
  include_bytes!("../../fixtures/advertisement-uris.bin");
/// An `ls-refs` request: `peel`, `symrefs`, `unborn` and two prefixes.
pub(crate) const LS_REFS_REQUEST: &[u8] = include_bytes!("../../fixtures/ls-refs.request.bin");
/// Its answer.
pub(crate) const LS_REFS_RESPONSE: &[u8] = include_bytes!("../../fixtures/ls-refs.response.bin");
/// A `fetch` of `main` with no haves (a clone).
pub(crate) const FETCH_CLONE_REQUEST: &[u8] =
  include_bytes!("../../fixtures/fetch-clone.request.bin");
/// Its answer: the packfile section alone.
pub(crate) const FETCH_CLONE_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/fetch-clone.response.bin");
/// A `fetch` with a have and no `done`.
pub(crate) const FETCH_HAVES_REQUEST: &[u8] =
  include_bytes!("../../fixtures/fetch-haves.request.bin");
/// Its answer: acknowledgments ending `ready`, a delimiter, the packfile section.
pub(crate) const FETCH_HAVES_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/fetch-haves.response.bin");
/// A `fetch` with `deepen 1`.
pub(crate) const FETCH_SHALLOW_REQUEST: &[u8] =
  include_bytes!("../../fixtures/fetch-shallow.request.bin");
/// Its answer: shallow-info, a delimiter, the packfile section.
pub(crate) const FETCH_SHALLOW_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/fetch-shallow.response.bin");
/// A `fetch` by `want-ref refs/heads/side`.
pub(crate) const FETCH_REF_IN_WANT_REQUEST: &[u8] =
  include_bytes!("../../fixtures/fetch-ref-in-want.request.bin");
/// Its answer: wanted-refs, a delimiter, the packfile section.
pub(crate) const FETCH_REF_IN_WANT_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/fetch-ref-in-want.response.bin");
/// A `fetch` that leaves progress on.
pub(crate) const FETCH_PROGRESS_REQUEST: &[u8] =
  include_bytes!("../../fixtures/fetch-progress.request.bin");
/// Its answer: the packfile section with progress on band 2 (its words vary between runs; its framing does not).
pub(crate) const FETCH_PROGRESS_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/fetch-progress.response.bin");
/// A `fetch` asking `sideband-all` and `packfile-uris https`.
pub(crate) const FETCH_URIS_REQUEST: &[u8] =
  include_bytes!("../../fixtures/fetch-uris.request.bin");
/// Its answer, every data packet on band 1: packfile-uris, a delimiter, the packfile section.
pub(crate) const FETCH_URIS_RESPONSE: &[u8] =
  include_bytes!("../../fixtures/fetch-uris.response.bin");

/// Every pkt-line stream stock git wrote or read, named.
pub(crate) const STREAMS: [(&str, &[u8]); 16] = [
  ("advertisement", ADVERTISEMENT),
  ("advertisement-uris", ADVERTISEMENT_URIS),
  ("ls-refs request", LS_REFS_REQUEST),
  ("ls-refs response", LS_REFS_RESPONSE),
  ("fetch-clone request", FETCH_CLONE_REQUEST),
  ("fetch-clone response", FETCH_CLONE_RESPONSE),
  ("fetch-haves request", FETCH_HAVES_REQUEST),
  ("fetch-haves response", FETCH_HAVES_RESPONSE),
  ("fetch-shallow request", FETCH_SHALLOW_REQUEST),
  ("fetch-shallow response", FETCH_SHALLOW_RESPONSE),
  ("fetch-ref-in-want request", FETCH_REF_IN_WANT_REQUEST),
  ("fetch-ref-in-want response", FETCH_REF_IN_WANT_RESPONSE),
  ("fetch-progress request", FETCH_PROGRESS_REQUEST),
  ("fetch-progress response", FETCH_PROGRESS_RESPONSE),
  ("fetch-uris request", FETCH_URIS_REQUEST),
  ("fetch-uris response", FETCH_URIS_RESPONSE),
];

/// Every fetch response whose packfile section is multiplexed after a plain `packfile` line, named.
pub(crate) const PACKFILE_RESPONSES: [(&str, &[u8]); 5] = [
  ("fetch-clone", FETCH_CLONE_RESPONSE),
  ("fetch-haves", FETCH_HAVES_RESPONSE),
  ("fetch-shallow", FETCH_SHALLOW_RESPONSE),
  ("fetch-ref-in-want", FETCH_REF_IN_WANT_RESPONSE),
  ("fetch-progress", FETCH_PROGRESS_RESPONSE),
];
