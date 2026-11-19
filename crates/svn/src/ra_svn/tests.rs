//! The ra_svn item codec's tests (A-531, §4.17.4 "5. The wire", ra_svn): golden vectors from the protocol document
//! (`subversion/libsvn_ra_svn/protocol`, Subversion 1.14.x at `d434dc14`, cited by section and line) and, where the
//! deployed server writes another form, from `svnserve`; every bound shown tight against the data it was derived from;
//! every hostile shape; and properties over generated items.

use super::*;
use proptest::prelude::*;

/// A word, borrowed from a literal.
fn w(word: &'static str) -> Value<'static> {
  Value::Word(word)
}

/// A number.
fn n(number: u64) -> Value<'static> {
  Value::Number(number)
}

/// A string, borrowed from a literal.
fn s(bytes: &'static [u8]) -> Value<'static> {
  Value::String(bytes)
}

/// A list.
fn l(items: Vec<Value<'static>>) -> Value<'static> {
  Value::List(items)
}

/// `protocol` §1's example item (19-22: "( word 22 6:string ( sublist ) )"), ended by the space Subversion's writer
/// writes after every item.
const SECTION_1_EXAMPLE: &[u8] = b"( word 22 6:string ( sublist ) ) ";

/// The same item ended as the document ends it: "a newline provides the terminating whitespace for the outer list"
/// (§1, 21-22).
const SECTION_1_EXAMPLE_LF: &[u8] = b"( word 22 6:string ( sublist ) )\n";

/// The value of `protocol` §1's example.
fn section_1_example() -> Value<'static> {
  l(vec![w("word"), n(22), s(b"string"), l(vec![w("sublist")])])
}

/// §1's command response in its failure form (`( failure ( err:error ... ) )`, `error: ( apr-err:number
/// message:string file:string line:number )`): a commit's conflict, `E160024`.
const FAILURE: &[u8] = b"( failure ( ( 160024 20:Conflict at '/trunk' 8:commit.c 42 ) ) ) ";

/// §2's greeting (101: `greeting: ( minver:number maxver:number mechs:list ( cap:word ... ) )`) inside a success
/// response, with the capability words `svnserve` 1.14 announces (`serve.c` 4239-4256).
const GREETING: &[u8] = b"( success ( 2 2 ( ) ( edit-pipeline svndiff1 accepts-svndiff2 absent-entries \
commit-revprops depth log-revprops atomic-revprops partial-replay inherited-props ephemeral-txnprops file-revs-reverse \
list ) ) ) ";

/// §2's client response (112-113: `response: ( version:number ( cap:word ... ) url:string ? ra-client:string ( ?
/// client:string ) )`).
const CLIENT_RESPONSE: &[u8] =
  b"( 2 ( edit-pipeline svndiff1 accepts-svndiff2 absent-entries depth mergeinfo \
log-revprops ) 29:svn://example.org/repos/trunk 10:SVN/1.14.5 ( ) ) ";

/// §2's auth request (126: `auth-request: ( ( mech:word ... ) realm:string )`) inside a success response.
const AUTH_REQUEST: &[u8] =
  b"( success ( ( ANONYMOUS CRAM-MD5 ) 36:2f8a43a4-6c31-4b54-9d9e-6c0f1b0e6b01 ) ) ";

/// §2's auth response (134: `auth-response: ( mech:word [ token:string ] )`), an anonymous one with an empty token.
const AUTH_RESPONSE: &[u8] = b"( ANONYMOUS ( 0: ) ) ";

/// §3.1.1's `get-latest-rev` (258-264: `params: ( )`) as a command (`command: ( command-name:word params:list )`).
const GET_LATEST_REV: &[u8] = b"( get-latest-rev ( ) ) ";

/// §3.1.1's `get-latest-rev` response (`response: ( rev:number )`) inside a success response.
const LATEST_REV: &[u8] = b"( success ( 1202 ) ) ";

/// §3.1.1's `commit-info` (304-305) as `svnserve` writes it (`serve.c` 1576, `"r(?c)(?c)(?c)"`): the date, the
/// author and the post-commit error each in an optional tuple, which the document's prototype does not show.
const COMMIT_INFO: &[u8] = b"( 1203 ( 27:2026-11-18T23:40:40.000000Z ) ( 5:carol ) ( ) ) ";

/// §3.1.2's `open-root` (536-537: `params: ( [ rev:number ] root-token:string )`).
const OPEN_ROOT: &[u8] = b"( open-root ( ( 1202 ) 2:d0 ) ) ";

/// §3.1.2's `textdelta-chunk` (568-569: `params: ( file-token:string chunk:string )`), its chunk the start of an
/// svndiff: a string holds any bytes, `(`, `)`, spaces and NULs among them.
const TEXTDELTA_CHUNK: &[u8] = b"( textdelta-chunk ( 2:c1 8:SVN\x00\x00\x0c\x10( ) ) ";

/// §3.1.3's `set-path` (590-592) with no lock token, which §4.2 (656-658) says is "an empty tuple" still sent.
const SET_PATH: &[u8] = b"( set-path ( 0: 1200 false ( ) infinity ) ) ";

/// Every golden vector with its value.
fn golden() -> Vec<(&'static [u8], Value<'static>)> {
  vec![
    (SECTION_1_EXAMPLE, section_1_example()),
    (
      FAILURE,
      l(vec![
        w("failure"),
        l(vec![l(vec![
          n(160_024),
          s(b"Conflict at '/trunk'"),
          s(b"commit.c"),
          n(42),
        ])]),
      ]),
    ),
    (
      GREETING,
      l(vec![
        w("success"),
        l(vec![
          n(2),
          n(2),
          l(vec![]),
          l(vec![
            w("edit-pipeline"),
            w("svndiff1"),
            w("accepts-svndiff2"),
            w("absent-entries"),
            w("commit-revprops"),
            w("depth"),
            w("log-revprops"),
            w("atomic-revprops"),
            w("partial-replay"),
            w("inherited-props"),
            w("ephemeral-txnprops"),
            w("file-revs-reverse"),
            w("list"),
          ]),
        ]),
      ]),
    ),
    (
      CLIENT_RESPONSE,
      l(vec![
        n(2),
        l(vec![
          w("edit-pipeline"),
          w("svndiff1"),
          w("accepts-svndiff2"),
          w("absent-entries"),
          w("depth"),
          w("mergeinfo"),
          w("log-revprops"),
        ]),
        s(b"svn://example.org/repos/trunk"),
        s(b"SVN/1.14.5"),
        l(vec![]),
      ]),
    ),
    (
      AUTH_REQUEST,
      l(vec![
        w("success"),
        l(vec![
          l(vec![w("ANONYMOUS"), w("CRAM-MD5")]),
          s(b"2f8a43a4-6c31-4b54-9d9e-6c0f1b0e6b01"),
        ]),
      ]),
    ),
    (AUTH_RESPONSE, l(vec![w("ANONYMOUS"), l(vec![s(b"")])])),
    (GET_LATEST_REV, l(vec![w("get-latest-rev"), l(vec![])])),
    (LATEST_REV, l(vec![w("success"), l(vec![n(1202)])])),
    (
      COMMIT_INFO,
      l(vec![
        n(1203),
        l(vec![s(b"2026-11-18T23:40:40.000000Z")]),
        l(vec![s(b"carol")]),
        l(vec![]),
      ]),
    ),
    (
      OPEN_ROOT,
      l(vec![w("open-root"), l(vec![l(vec![n(1202)]), s(b"d0")])]),
    ),
    (
      TEXTDELTA_CHUNK,
      l(vec![
        w("textdelta-chunk"),
        l(vec![s(b"c1"), s(b"SVN\x00\x00\x0c\x10(")]),
      ]),
    ),
    (
      SET_PATH,
      l(vec![
        w("set-path"),
        l(vec![s(b""), n(1200), w("false"), l(vec![]), w("infinity")]),
      ]),
    ),
  ]
}

/// Reads one item from `bytes`, held whole, within `bounds`.
fn read(bytes: &[u8], bounds: Bounds) -> Result<Parsed<'_>, ItemError> {
  Parser::new(bounds).parse(bytes)
}

/// What `write` writes for `value`.
fn written(value: &Value<'_>) -> Vec<u8> {
  let mut out = Vec::new();
  write(value, &mut out).unwrap();
  out
}

/// `bounds` with one field lowered by one, or `None` when that field is already zero.
fn lowered(bounds: Bounds, field: usize) -> Option<Bounds> {
  let mut lower = bounds;
  let slot = match field {
    0 => &mut lower.item_bytes,
    1 => &mut lower.word_bytes,
    2 => &mut lower.depth,
    _ => &mut lower.nodes,
  };
  *slot = slot.checked_sub(1)?;
  Some(lower)
}

/// Whether `error` is the refusal a bound lowered at `field` makes.
fn refused_by(error: ItemError, field: usize) -> bool {
  matches!(
    (field, error),
    (0, ItemError::ItemTooLarge { .. })
      | (1, ItemError::WordTooLong { .. })
      | (2, ItemError::NestedTooDeep { .. })
      | (3, ItemError::TooManyItems { .. })
  )
}

/// A-531 (golden vectors, `protocol` §1, §2, §3.1.1-§3.1.3, §4.2; `serve.c` 1576 and 4239-4256): read each vector
/// within the tightest bounds of its value, expect the value taking every byte; write the value, expect the vector byte
/// for byte; and measure the value, expect the vector's length.
#[test]
fn the_protocols_items_read_and_write_as_the_document_and_svnserve_give_them() {
  for (bytes, value) in golden() {
    let bounds = Bounds::of(&value).unwrap();
    assert_eq!(
      bounds.item_bytes,
      bytes.len(),
      "{}",
      String::from_utf8_lossy(bytes)
    );
    assert_eq!(
      read(bytes, bounds),
      Ok(Parsed::Item {
        value: value.clone(),
        consumed: bytes.len()
      }),
      "{}",
      String::from_utf8_lossy(bytes)
    );
    assert_eq!(written(&value), bytes);
  }
}

/// A-531 (golden vector, `protocol` §1 21-22): read the example ended by a line feed, as the document ends it, expect
/// the same value; written again it ends with a space, as Subversion's writer ends every item.
#[test]
fn an_item_ended_by_a_line_feed_reads_as_one_ended_by_a_space() {
  let bounds = Bounds::of(&section_1_example()).unwrap();
  assert_eq!(
    read(SECTION_1_EXAMPLE_LF, bounds),
    Ok(Parsed::Item {
      value: section_1_example(),
      consumed: SECTION_1_EXAMPLE_LF.len()
    })
  );
  assert_eq!(written(&section_1_example()), SECTION_1_EXAMPLE);
}

/// A-531 (every bound derived from the data, each one effective): read each golden vector within its tightest bounds
/// with one bound lowered by one, expect the refusal that bound makes: the item too large, a word too long, a list too
/// deep, too many items.
#[test]
fn each_bound_derived_from_a_vector_is_exactly_enough() {
  for (bytes, value) in golden() {
    let bounds = Bounds::of(&value).unwrap();
    let mut lowered_and_refused = 0;
    for field in 0..4 {
      let Some(lower) = lowered(bounds, field) else {
        continue;
      };
      let outcome = read(bytes, lower);
      assert!(
        matches!(outcome, Err(error) if refused_by(error, field)),
        "{} with field {field} lowered: {outcome:?}",
        String::from_utf8_lossy(bytes)
      );
      lowered_and_refused += 1;
    }
    // Every item takes bytes and is at least one item, so those two bounds at least were lowered and refused: a
    // measure of nothing would leave this test nothing to check (the stub's run passed it so, before this line).
    assert!(
      lowered_and_refused >= 2,
      "{} measured {bounds:?}",
      String::from_utf8_lossy(bytes)
    );
  }
}

/// A-531 (hostile input, len = u32::MAX): read a string declaring 4,294,967,295 bytes, expect it refused at its `:`,
/// before one of its bytes is waited for, within the bounds of §1's example; and the same for 2^64 - 1 bytes, and a
/// length past 64 bits refused at its last digit.
#[test]
fn a_string_declaring_more_than_its_bound_is_refused_at_its_colon() {
  let bounds = Bounds::of(&section_1_example()).unwrap();
  assert_eq!(
    read(b"4294967295:", bounds),
    Err(ItemError::ItemTooLarge { at: 10 })
  );
  assert_eq!(
    read(b"18446744073709551615:", bounds),
    Err(ItemError::ItemTooLarge { at: 20 })
  );
  assert_eq!(
    read(b"18446744073709551616:", bounds),
    Err(ItemError::NumberTooLarge { at: 19 })
  );
  assert_eq!(
    ItemError::ItemTooLarge { at: 10 }.subversion_code(),
    210_009
  );
}

/// A-531 (numbers at their limit, `protocol` §1 `number = 1*DIGIT`): read 2^64 - 1, expect it; read 2^64, expect it
/// refused exactly, where the deployed parser's check (`marshal.c` `read_item`) misses some values past 64 bits.
#[test]
fn a_number_past_sixty_four_bits_is_refused_exactly() {
  let largest = n(u64::MAX);
  let bounds = Bounds::of(&largest).unwrap();
  assert_eq!(
    read(b"18446744073709551615 ", bounds),
    Ok(Parsed::Item {
      value: largest,
      consumed: 21
    })
  );
  let wider = Bounds {
    item_bytes: bounds.item_bytes + 1,
    ..bounds
  };
  assert_eq!(
    read(b"18446744073709551616 ", wider),
    Err(ItemError::NumberTooLarge { at: 19 })
  );
  assert_eq!(
    ItemError::NumberTooLarge { at: 19 }.subversion_code(),
    210_004
  );
}

/// A-531 (hostile input, truncated items): read every proper prefix of every golden vector, expect a call for more
/// bytes than it holds and no more than the item's bound, and an end of stream there refused as truncated.
#[test]
fn every_truncated_vector_asks_for_more_and_is_refused_at_the_end() {
  for (bytes, value) in golden() {
    let bounds = Bounds::of(&value).unwrap();
    for cut in 1..bytes.len() {
      let mut parser = Parser::new(bounds);
      match parser.parse(&bytes[..cut]) {
        Ok(Parsed::NeedMore { needed }) => {
          assert!(
            needed > cut && needed <= bounds.item_bytes,
            "cut {cut}: needed {needed}"
          );
        }
        other => panic!("{} cut at {cut}: {other:?}", String::from_utf8_lossy(bytes)),
      }
      assert_eq!(parser.finish(), Err(ItemError::Truncated));
    }
  }
  assert_eq!(Parser::new(Bounds::default()).finish(), Ok(()));
  assert_eq!(ItemError::Truncated.subversion_code(), 210_002);
}

/// A-531 (hostile input, nesting past the bound): read three nested lists within depth 2, expect the third `(`
/// refused; within depth 3, expect them read; read a million `(` within the deepest golden vector's depth, expect a
/// refusal at the first past it, in one pass.
#[test]
fn nesting_past_the_bound_is_refused_at_its_open() {
  let nested = l(vec![l(vec![l(vec![])])]);
  let bounds = Bounds::of(&nested).unwrap();
  assert_eq!(
    read(b"( ( ( ) ) ) ", bounds),
    Ok(Parsed::Item {
      value: nested,
      consumed: 12
    })
  );
  let shallower = Bounds { depth: 2, ..bounds };
  assert_eq!(
    read(b"( ( ( ) ) ) ", shallower),
    Err(ItemError::NestedTooDeep { at: 4 })
  );
  let deepest = golden()
    .iter()
    .map(|(_, value)| Bounds::of(value).unwrap().depth)
    .max()
    .unwrap();
  let flood = vec![b'('; 1_000_000];
  let generous = Bounds {
    item_bytes: flood.len() + 1,
    word_bytes: 0,
    depth: deepest,
    nodes: flood.len(),
  };
  assert_eq!(
    read(&flood, generous),
    Err(ItemError::NestedTooDeep { at: deepest })
  );
  assert_eq!(
    ItemError::NestedTooDeep { at: 0 }.subversion_code(),
    210_004
  );
}

/// A-531 (hostile input, foreign bytes): read bytes no item starts with (`#`, `{`, a tab and a carriage return, which
/// are not the grammar's whitespace, NUL, 0xff, a `)` with no list open, a word starting with `-`, a non-ASCII
/// letter), expect each refused where it stands; read items not followed by whitespace, expect the byte after each
/// refused.
#[test]
fn foreign_bytes_and_items_without_whitespace_are_refused_where_they_stand() {
  let bounds = Bounds::of(&section_1_example()).unwrap();
  let foreign: [&[u8]; 9] = [
    b"#",
    b"{",
    b"\t",
    b"\r",
    b"\x00",
    b"\xff",
    b")",
    b"-word ",
    "\u{e9}t\u{e9} ".as_bytes(),
  ];
  for bytes in foreign {
    assert_eq!(
      read(bytes, bounds),
      Err(ItemError::Malformed { at: 0 }),
      "{bytes:?}"
    );
  }
  let unended: [(&[u8], usize); 6] = [
    (b"(word) ", 5),
    (b"( word)", 6),
    (b"22x ", 2),
    (b"6:string)", 8),
    (b"( )x", 3),
    (b"( word\t) ", 6),
  ];
  for (bytes, at) in unended {
    assert_eq!(
      read(bytes, bounds),
      Err(ItemError::Malformed { at }),
      "{}",
      String::from_utf8_lossy(bytes)
    );
  }
  assert_eq!(ItemError::Malformed { at: 0 }.subversion_code(), 210_004);
}

/// A-531 (the word bound, `protocol` §4.1 627 and `marshal.c` 62-66): read a word of 24 letters within a bound derived
/// from it, expect it read, as a stock 1.14 parser reads it (its `MAX_WORD_LENGTH` 25 refuses 25 and more, stricter
/// than the document's 31); read 25 letters within the same bound, expect the 25th refused.
#[test]
fn a_word_past_the_word_bound_is_refused_at_the_letter_past_it() {
  let longest = w("abcdefghijklmnopqrstuvwx");
  let too_long: &[u8] = b"abcdefghijklmnopqrstuvwxy ";
  let bounds = Bounds {
    item_bytes: too_long.len(),
    ..Bounds::of(&longest).unwrap()
  };
  assert_eq!(bounds.word_bytes, 24);
  assert_eq!(
    read(b"abcdefghijklmnopqrstuvwx ", bounds),
    Ok(Parsed::Item {
      value: longest,
      consumed: 25
    })
  );
  assert_eq!(
    read(too_long, bounds),
    Err(ItemError::WordTooLong { at: 24 })
  );
}

/// A-531 (the item and byte bounds): read a list of three words within a bound of three items, expect the third word
/// refused (the list is the first item); read an item a byte longer than its bound, expect the byte past it refused.
#[test]
fn too_many_items_or_bytes_are_refused_where_they_pass_the_bound() {
  let three = l(vec![w("a"), w("b"), w("c")]);
  let bounds = Bounds::of(&three).unwrap();
  assert_eq!(bounds.nodes, 4);
  assert_eq!(
    read(b"( a b c ) ", Bounds { nodes: 3, ..bounds }),
    Err(ItemError::TooManyItems { at: 6 })
  );
  let abc = l(vec![w("abc")]);
  let tight = Bounds::of(&abc).unwrap();
  assert_eq!(tight.item_bytes, 8);
  assert_eq!(
    read(
      b"( abc ) ",
      Bounds {
        item_bytes: 7,
        ..tight
      }
    ),
    Err(ItemError::ItemTooLarge { at: 7 })
  );
  assert_eq!(ItemError::TooManyItems { at: 6 }.subversion_code(), 210_009);
}

/// A-531: read two items from one buffer, leading whitespace before the first, expect the first with the whitespace
/// counted in what it consumed, then, its bytes dropped, the second.
#[test]
fn items_follow_one_another_in_one_buffer() {
  let bytes = b"\n  ( a ) ( b ) ";
  let bounds = Bounds {
    item_bytes: bytes.len(),
    word_bytes: 1,
    depth: 1,
    nodes: 2,
  };
  let mut parser = Parser::new(bounds);
  assert_eq!(
    parser.parse(bytes),
    Ok(Parsed::Item {
      value: l(vec![w("a")]),
      consumed: 9
    })
  );
  assert_eq!(
    parser.parse(&bytes[9..]),
    Ok(Parsed::Item {
      value: l(vec![w("b")]),
      consumed: 6
    })
  );
  assert_eq!(parser.finish(), Ok(()));
}

/// A-531 (the writer): write words outside the grammar (empty, a digit first, a space, an underscore, a non-ASCII
/// letter), expect each refused.
#[test]
fn a_word_outside_the_grammar_is_never_written() {
  for word in ["", "1abc", "a b", "a_b", "\u{e9}t\u{e9}"] {
    assert_eq!(
      write(&Value::Word(word), &mut Vec::new()),
      Err(ItemError::InvalidWord),
      "{word:?}"
    );
  }
}

/// An item as the properties build it, owning its words and strings.
#[derive(Clone, Debug)]
enum Owned {
  Word(String),
  Number(u64),
  String(Vec<u8>),
  List(Vec<Owned>),
}

impl Owned {
  /// The value it is.
  fn view(&self) -> Value<'_> {
    match self {
      Self::Word(word) => Value::Word(word),
      Self::Number(number) => Value::Number(*number),
      Self::String(bytes) => Value::String(bytes),
      Self::List(items) => Value::List(items.iter().map(Owned::view).collect()),
    }
  }
}

/// Words of the grammar: a letter, then letters, digits and `-`.
fn words() -> impl Strategy<Value = String> {
  let letters: Vec<char> = ('a'..='z').chain('A'..='Z').collect();
  let rest: Vec<char> = letters
    .iter()
    .copied()
    .chain('0'..='9')
    .chain(['-'])
    .collect();
  (
    prop::sample::select(letters),
    prop::collection::vec(prop::sample::select(rest), 0..16),
  )
    .prop_map(|(first, rest)| std::iter::once(first).chain(rest).collect())
}

/// Items of every kind, nested.
#[allow(
  clippy::disallowed_types,
  reason = "D-8's test-harness exception: proptest's `prop_oneof!` and `prop_recursive` build their union from `Arc`s, inside the macro; nothing in slates shares by `Arc`"
)]
fn items() -> impl Strategy<Value = Owned> {
  let leaf = prop_oneof![
    words().prop_map(Owned::Word),
    any::<u64>().prop_map(Owned::Number),
    prop::collection::vec(any::<u8>(), 0..24).prop_map(Owned::String),
  ];
  leaf.prop_recursive(4, 48, 6, |inner| {
    prop::collection::vec(inner, 0..6).prop_map(Owned::List)
  })
}

/// Bounds from the length of arbitrary bytes alone: the most such bytes could need.
fn bounds_of_len(len: usize) -> Bounds {
  Bounds {
    item_bytes: len,
    word_bytes: len,
    depth: len,
    nodes: len,
  }
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::default()))]

  /// A-531 (oracle: the writer and the reader agree): write any item, expect its measured length; read it within its
  /// tightest bounds, expect it whole; lower any one bound by one, expect that bound's refusal.
  #[test]
  fn any_item_written_reads_back_within_exactly_its_bounds(owned in items()) {
    let value = owned.view();
    let bytes = written(&value);
    let bounds = Bounds::of(&value).unwrap();
    prop_assert_eq!(bounds.item_bytes, bytes.len());
    prop_assert_eq!(read(&bytes, bounds), Ok(Parsed::Item { value: value.clone(), consumed: bytes.len() }));
    for field in 0..4 {
      if let Some(lower) = lowered(bounds, field) {
        let outcome = read(&bytes, lower);
        prop_assert!(matches!(outcome, Err(error) if refused_by(error, field)), "field {}: {:?}", field, outcome);
      }
    }
  }

  /// A-531 (split invariance, one step a byte): feed any written item to one parser in any pieces, the buffer growing
  /// at its end, expect every call for more to ask for more than is held and no more than the bound, and the item whole.
  #[test]
  fn any_item_fed_in_any_pieces_reads_the_same(owned in items(), cuts in prop::collection::vec(any::<usize>(), 0..16)) {
    let value = owned.view();
    let bytes = written(&value);
    let bounds = Bounds::of(&value).unwrap();
    let mut ends: Vec<usize> = cuts.iter().map(|cut| cut % (bytes.len() + 1)).collect();
    ends.push(bytes.len());
    ends.sort_unstable();
    let mut parser = Parser::new(bounds);
    let mut item = None;
    for end in ends {
      match parser.parse(&bytes[..end]).unwrap() {
        Parsed::NeedMore { needed } => prop_assert!(needed > end && needed <= bounds.item_bytes),
        Parsed::Item { value, consumed } => {
          prop_assert_eq!(consumed, bytes.len());
          item = Some(value);
          break;
        }
      }
    }
    prop_assert_eq!(item, Some(value));
  }

  /// A-531 (hostile input, any bytes): read any bytes within bounds derived from their length, expect a typed answer
  /// and never a panic; an item read writes again and reads back equal.
  #[test]
  fn any_bytes_read_to_a_typed_answer(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
    if let Ok(Parsed::Item { value, .. }) = read(&bytes, bounds_of_len(bytes.len())) {
      let again = written(&value);
      prop_assert_eq!(read(&again, bounds_of_len(again.len())), Ok(Parsed::Item { value, consumed: again.len() }));
    }
  }
}
