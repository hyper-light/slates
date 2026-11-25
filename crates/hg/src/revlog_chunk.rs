//! A revlog's chunk as stock Mercurial 7.2 stores it (goal 93 perf, the stream's size; main's ruling of 2026-11-25):
//! the one place slates makes a revision's stored chunk from its plain bytes and decides whether a revlog is inline, for
//! every writer of revlogs (a rendered store, a stream's count and send passes, and later the changeset store's intake,
//! w-hg's piece (b), which writes each chunk once at arrival).
//!
//! **Decoded** ([`decode`]): every kind 7.2 reads, zlib's too, through the revlog reader's own decoder.
//!
//! **The chunk** (`revlog.py` `revlog.compress` over `utils/compression.py` `zstdrevlogcompressor`, 7.2's default
//! engine since `format.revlog-compression` names zstd first): an empty text is stored empty; a text of 50 bytes or more
//! is one zstd frame at level 3 with its content's size, when that frame is shorter than the text; otherwise the text as
//! it is where it begins with a NUL (a bdiff delta does, its first hunk's start below 2^24), and `u` then the text
//! elsewhere. 7.2 compresses a text over 1,000,000 bytes as a stream of frames' pieces whose frame names no size; slates
//! writes one frame naming its size there too, which every reader of a zstd chunk decodes alike (7.2's
//! `zstdrevlogcompressor.decompress` reads a frame either way, and so does `crate::revlog`).
//!
//! **Inline** (`revlog.py` `_enforceinlinesize`, `_maxinline`): a revlog may keep its chunks in its index file, each
//! after its entry, while they hold under 131,072 bytes; past that 7.2 splits it into an index file and a data file and
//! never joins them again. A changelog is never inline (`changelog.py` opens it `may_inline=False`). A store rendered
//! whole is split exactly where 7.2's would be once the same chunks landed, since chunks only grow a log.
//!
//! **One state a thread.** The zstd context (its tables, a few MiB at level 3) and the buffer a frame is made in are
//! kept on the calling thread (a shard's; the runtime is thread-per-core), made at its first chunk and kept until
//! [`let_go`], so a stream makes one, never one a revision. Measured on git's deflate states (A-868): one a unit made a
//! clone's states O(units). The buffer is at most the largest chunk's zstd bound since the last let-go.
//!
//! **Counted.** [`take_counts`]: each pass's chunks, plain and stored bytes and nanoseconds on the clock the daemon
//! installs ([`install_clock`]; none installed, the nanoseconds stay zero), on this thread since the last call; the
//! server counts them on its shard (`vcs_store.hg_stream_compress_*`), so the per-stream compression piece (b) removes
//! has a number to beat.

use std::cell::{Cell, RefCell};
use std::sync::OnceLock;

use zstd::zstd_safe::{self, CCtx, CParameter};

use crate::read::to_u64;

/// Format: 7.2's zstd level for revlog chunks (`storage.revlog.zstd.level`'s default, `zstdrevlogcompressor`'s
/// `level=3`).
pub const ZSTD_LEVEL: i32 = 3;
/// Format: the shortest text 7.2's zstd revlog compressor tries (`zstdrevlogcompressor.compress`, `insize < 50`).
pub const SHORTEST_COMPRESSED: usize = 50;
/// Format: the chunk bytes a revlog may hold and stay inline (`revlog.py` `_maxinline`, its split at `total_size <
/// _maxinline` failing).
pub const MAX_INLINE_DATA: u64 = 131_072;
/// Format: the header of a chunk stored as it is (`revlog.compress`'s `u`).
const UNCOMPRESSED: u8 = b'u';

/// Which writer a chunk was made for, so each is counted apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
  /// A stream's count: its chunks' lengths, for the files' sizes its head names first.
  Count,
  /// A stream's or a rendering's write: the chunks themselves.
  Send,
}

/// One pass's chunks since the last [`take_counts`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassCounts {
  /// Chunks made.
  pub chunks: u64,
  /// Their plain bytes.
  pub plain_bytes: u64,
  /// Their stored bytes, headers in.
  pub stored_bytes: u64,
  /// Nanoseconds making them, on the installed clock.
  pub ns: u64,
}

/// Each pass's counts on this thread since the last [`take_counts`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChunkCounts {
  /// A stream's count pass.
  pub count: PassCounts,
  /// A stream's send pass and a rendering.
  pub send: PassCounts,
}

/// Why a chunk could not be made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkRefused {
  /// No zstd context or buffer could be had: libzstd's allocation failed, or this thread's state was already lent
  /// (a chunk asked inside another's making, which no caller does).
  NoState,
  /// libzstd refused the frame: its error's name.
  Engine(&'static str),
}

/// A chunk's form (the module doc's rule), its bytes borrowed from the plain text or the thread's buffer.
enum Stored<'s> {
  /// The plain text, stored empty.
  Empty,
  /// One zstd frame.
  Framed(&'s [u8]),
  /// The plain text as it is: it begins with a NUL.
  Raw(&'s [u8]),
  /// `u` and the plain text.
  Marked(&'s [u8]),
}

impl Stored<'_> {
  /// Its stored length, header in.
  fn len(&self) -> u64 {
    match self {
      Stored::Empty => 0,
      Stored::Framed(bytes) | Stored::Raw(bytes) => to_u64(bytes.len()),
      Stored::Marked(bytes) => to_u64(bytes.len()).saturating_add(1),
    }
  }

  /// Appends its stored bytes to `out`.
  fn put(&self, out: &mut Vec<u8>) {
    match self {
      Stored::Empty => {}
      Stored::Framed(bytes) | Stored::Raw(bytes) => out.extend_from_slice(bytes),
      Stored::Marked(bytes) => {
        out.push(UNCOMPRESSED);
        out.extend_from_slice(bytes);
      }
    }
  }
}

/// A thread's compression state: the context, made at the first chunk, and the buffer a frame is made in.
struct State {
  context: Option<CCtx<'static>>,
  frame: Vec<u8>,
}

thread_local! {
  static STATE: RefCell<State> = const {
    RefCell::new(State {
      context: None,
      frame: Vec::new(),
    })
  };
  static COUNTS: Cell<ChunkCounts> = const {
    Cell::new(ChunkCounts {
      count: PassCounts {
        chunks: 0,
        plain_bytes: 0,
        stored_bytes: 0,
        ns: 0,
      },
      send: PassCounts {
        chunks: 0,
        plain_bytes: 0,
        stored_bytes: 0,
        ns: 0,
      },
    })
  };
}

/// The clock chunks are timed on: the daemon's, installed once ([`install_clock`]).
static CLOCK: OnceLock<fn() -> u64> = OnceLock::new();

/// Installs the clock every thread's chunks are timed on, in nanoseconds (the daemon's shard clock); whether this call
/// installed it (only the first does).
pub fn install_clock(clock: fn() -> u64) -> bool {
  CLOCK.set(clock).is_ok()
}

/// The installed clock's reading; zero with none.
fn now_ns() -> u64 {
  CLOCK.get().map_or(0, |clock| clock())
}

/// Appends `plain`'s stored chunk to `out` (the module doc's rule), counted under `pass`: its stored length.
///
/// # Errors
/// [`ChunkRefused`] where no zstd state could be had or libzstd refused the frame; `out` is left as it was.
pub fn chunk(plain: &[u8], pass: Pass, out: &mut Vec<u8>) -> Result<u64, ChunkRefused> {
  stored(plain, pass, |stored| {
    stored.put(out);
    stored.len()
  })
}

/// `plain`'s stored chunk's length (the module doc's rule), counted under `pass`, the chunk itself let go.
///
/// # Errors
/// As [`chunk`].
pub fn chunk_len(plain: &[u8], pass: Pass) -> Result<u64, ChunkRefused> {
  stored(plain, pass, |stored| stored.len())
}

/// `stored`, a chunk of revision `rev` (named in a refusal), decoded to its plain bytes by its header, every kind 7.2
/// reads: none, `u`, a leading NUL, zlib's `x` and zstd's `(` (`crate::revlog::decode_chunk`, the reader's own), each
/// byte charged to `admission`.
///
/// # Errors
/// As [`crate::revlog::decode_chunk`].
pub fn decode(
  stored: &[u8],
  rev: u32,
  admission: &mut crate::Admission,
) -> Result<Vec<u8>, crate::revlog::Refused> {
  crate::revlog::decode_chunk(stored, rev, admission)
}

/// `stored`, a chunk as a revlog keeps it, decoded into at most `most` bytes: its decoder's own memory charged apart
/// ([`crate::compression`]'s `decoder_held`), so `most` bounds what it makes; none where it does not decode or makes
/// more. The changeset store's reader of a text held as a chunk (goal 93, main's ruling (b) of 2026-11-25), installed by
/// the daemon (`slates_store::texts::install_chunk_decoder`).
pub fn decode_within(stored: &[u8], most: u64) -> Option<Vec<u8>> {
  let engine = match stored.first() {
    Some(&crate::revlog::ZLIB) => crate::compression::Engine::Zlib,
    Some(&crate::revlog::ZSTD) => crate::compression::Engine::Zstd,
    _ => crate::compression::Engine::Uncompressed,
  };
  let held = crate::compression::decoder_held(engine, stored).ok()?;
  let mut admission = crate::Admission::new(most.saturating_add(held));
  let plain = decode(stored, 0, &mut admission).ok()?;
  (u64::try_from(plain.len()).ok()? <= most).then_some(plain)
}

/// Whether a revlog whose chunks hold `data` bytes is inline (the module doc): one that may be (any but a changelog)
/// under 7.2's [`MAX_INLINE_DATA`].
pub fn inline(may_inline: bool, data: u64) -> bool {
  may_inline && data < MAX_INLINE_DATA
}

/// `plain`'s chunk made and handed to `then`, the pass counted.
fn stored<T>(
  plain: &[u8],
  pass: Pass,
  then: impl FnOnce(&Stored<'_>) -> T,
) -> Result<T, ChunkRefused> {
  let start = now_ns();
  let made = STATE.with(|state| {
    let mut state = state.try_borrow_mut().map_err(|_| ChunkRefused::NoState)?;
    let State { context, frame } = &mut *state;
    let stored = if plain.is_empty() {
      Stored::Empty
    } else if plain.len() >= SHORTEST_COMPRESSED && framed(context, frame, plain)? < plain.len() {
      Stored::Framed(frame.as_slice())
    } else if plain.first() == Some(&0) {
      Stored::Raw(plain)
    } else {
      Stored::Marked(plain)
    };
    let length = stored.len();
    Ok((then(&stored), length))
  });
  let (answer, length) = made?;
  let spent = now_ns().saturating_sub(start);
  COUNTS.with(|counts| {
    let mut now = counts.get();
    let at = match pass {
      Pass::Count => &mut now.count,
      Pass::Send => &mut now.send,
    };
    at.chunks = at.chunks.saturating_add(1);
    at.plain_bytes = at.plain_bytes.saturating_add(to_u64(plain.len()));
    at.stored_bytes = at.stored_bytes.saturating_add(length);
    at.ns = at.ns.saturating_add(spent);
    counts.set(now);
  });
  Ok(answer)
}

/// `plain` as one zstd frame at [`ZSTD_LEVEL`] naming its size, made in `frame` with the thread's context (made here at
/// its first use): the frame's length.
fn framed(
  context: &mut Option<CCtx<'static>>,
  frame: &mut Vec<u8>,
  plain: &[u8],
) -> Result<usize, ChunkRefused> {
  if context.is_none() {
    let mut made = CCtx::try_create().ok_or(ChunkRefused::NoState)?;
    made
      .set_parameter(CParameter::CompressionLevel(ZSTD_LEVEL))
      .map_err(|code| ChunkRefused::Engine(zstd_safe::get_error_name(code)))?;
    *context = Some(made);
  }
  let context = context.as_mut().ok_or(ChunkRefused::NoState)?;
  frame.clear();
  frame
    .try_reserve(zstd_safe::compress_bound(plain.len()))
    .map_err(|_| ChunkRefused::NoState)?;
  // `compress2` writes from the buffer's start and sets its length; a one-shot frame names its content's size, as
  // python-zstandard's `ZstdCompressor.compress` writes it.
  context
    .compress2(frame, plain)
    .map_err(|code| ChunkRefused::Engine(zstd_safe::get_error_name(code)))
}

/// This thread's counts since its last call, begun again at zero (the server counts them on its shard).
pub fn take_counts() -> ChunkCounts {
  COUNTS.with(|counts| counts.replace(ChunkCounts::default()))
}

/// Drops this thread's zstd context and buffer (a shard gone idle): the bytes the buffer held.
pub fn let_go() -> u64 {
  STATE
    .try_with(|state| {
      state.try_borrow_mut().map_or(0, |mut state| {
        state.context = None;
        to_u64(core::mem::take(&mut state.frame).capacity())
      })
    })
    .unwrap_or(0)
}
