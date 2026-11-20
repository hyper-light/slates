//! HTTPv2 commits (§4.17.4 item 5 "HTTP" and piece 13's write side; A-681 part C): what stock svn's `commit`, `mkdir`,
//! `copy`, `delete`, `import` and `propset` send over HTTP, answered as `mod_dav_svn` answers them, through the one
//! commit drive ra_svn's commits run ([`crate::commit::drive::Drive`], A-617).
//!
//! **The requests**, as ra_serf 1.15 sends them (`libsvn_ra_serf/commit.c`): a POST to `!svn/me` making the
//! transaction, its body the skel `( create-txn-with-props ( NAME VALUE ... ) )` (or `( create-txn )`), answered `201`
//! with `SVN-Txn-Name` (`posts/create_txn.c`); then, under the transaction's root `!svn/txr/NAME/PATH`, a PUT of each
//! file's text as svndiff (`X-SVN-Version-Name` naming an opened file's base revision, none for an added one;
//! `X-SVN-Base-Fulltext-MD5` and `X-SVN-Result-Fulltext-MD5` its checksums), a MKCOL of each added directory, a DELETE
//! of each deleted path (`X-SVN-Version-Name` its revision), a COPY of each copied path (`Destination` the
//! transaction path, the source a revision root's URL), a PROPPATCH of each node whose properties change; and last a
//! MERGE naming the transaction, which commits it, or a DELETE of `!svn/txn/NAME`, which aborts it.
//!
//! **The drive.** Each request becomes the editor commands ra_svn's client would send for it, handed to the drive
//! one at a time ([`Drive::edit`]), so every check, lock and refusal is the one ra_svn's commits meet: a path's parent
//! directories are opened once each (`open-dir`) and stay open until the MERGE closes them, deepest first, before
//! `close-edit`; a file is opened (`open-file` at the request's base revision, or with no revision where this
//! transaction added it) or added (`add-file`), its text applied as one delta whose chunks are the PUT's body, and
//! closed with the result checksum. A hook due is answered by the caller ([`Hooks`]), at once. A refusal is answered
//! with its first error, `dav_svn__convert_err`'s status, and ends the transaction: what follows is refused `404`, as
//! `mod_dav_svn` answers a transaction that no longer exists.
//!
//! A PUT's body is handed to the drive a run at a time as it arrives ([`DavCommit::begin_put`], [`DavCommit::put_chunk`],
//! [`DavCommit::end_put`]), as `mod_dav_svn` streams it into FSFS, so a commit never holds a body whole (§4.17.1 "A
//! push's intake").
//!
//! Stricter than `mod_dav_svn`, each stated: revision properties are taken from the POST alone (ra_serf 1.8
//! and later send them there; a PROPPATCH of `!svn/txn/NAME` is refused `501`).
//!
//! No I/O, no clock (the caller's instant in its setup), no randomness, no panic.

use std::collections::{BTreeMap, BTreeSet};

use super::locking::{HttpAnswer, convert_status, error_answer};
use super::xml::{Element, parse};
use super::{Bounds, CUSTOM_PROPS, DAV, DavRefusal, SVN_DAV, SVN_PROPS};
use crate::commit::drive::{Drive, Due, Edited, Green, Setup, TextSink, Work};
use crate::commit::edit::{CopiedFrom, EditCommand};
use crate::index::{Index, NodeKind, PropertyPairs, Revision};
use crate::lock::LockTable;

/// `SVN_ERR_FS_NO_SUCH_TRANSACTION`: a request on a transaction that has ended.
/// Format: `svn_error_codes.h`.
const SVN_ERR_FS_NO_SUCH_TRANSACTION: u32 = 160_007;
/// `SVN_ERR_FS_NOT_FOUND`: a copy's source that does not exist.
/// Format: `svn_error_codes.h`.
const SVN_ERR_FS_NOT_FOUND: u32 = 160_013;
/// `SVN_ERR_UNSUPPORTED_FEATURE`: a request this site does not serve.
/// Format: `svn_error_codes.h`.
const SVN_ERR_UNSUPPORTED_FEATURE: u32 = 200_007;
/// `SVN_ERR_DAV_MALFORMED_DATA`: a body that cannot be read.
/// Format: `svn_error_codes.h`.
const SVN_ERR_DAV_MALFORMED_DATA: u32 = 175_009;
/// Created.
/// Format: RFC 9110 §15.3.2.
const HTTP_CREATED: u16 = 201;
/// No Content.
/// Format: RFC 9110 §15.3.5.
const HTTP_NO_CONTENT: u16 = 204;
/// Multi-Status.
/// Format: RFC 4918 §11.1.
const HTTP_MULTI_STATUS: u16 = 207;
/// Bad Request.
/// Format: RFC 9110 §15.5.1.
const HTTP_BAD_REQUEST: u16 = 400;
/// Not Found.
/// Format: RFC 9110 §15.5.5.
const HTTP_NOT_FOUND: u16 = 404;
/// Internal Server Error.
/// Format: RFC 9110 §15.6.1.
const HTTP_INTERNAL_SERVER_ERROR: u16 = 500;
/// Not Implemented.
/// Format: RFC 9110 §15.6.2.
const HTTP_NOT_IMPLEMENTED: u16 = 501;
/// The token the edit's root is opened with.
/// Format: ra_svn's first directory token, `d0`.
const ROOT_TOKEN: &[u8] = b"d0";
/// `svn:author`, which the principal's name replaces (`serve.c` 1526-1541).
/// Format: `svn_props.h` `SVN_PROP_REVISION_AUTHOR`.
const REVISION_AUTHOR: &[u8] = b"svn:author";
/// `svn:date`, which the commit step stamps.
/// Format: `svn_props.h` `SVN_PROP_REVISION_DATE`.
const REVISION_DATE: &[u8] = b"svn:date";
/// The prefix a property in Subversion's namespace is named by.
/// Format: `svn_props.h` `SVN_PROP_PREFIX`.
const SVN_PREFIX: &str = "svn:";
/// The scheme every Subversion lock token is written in (`svn_fs_fs__generate_lock_token`).
/// Format: RFC 4918 §20's `opaquelocktoken` URI scheme.
const LOCK_TOKEN_SCHEME: &str = "opaquelocktoken:";
/// The attribute naming a property value's encoding.
/// Format: `dav_svn__encode_xml`'s `V:encoding` (`SVN_DAV_PROP_NS_DAV`).
const ENCODING: &str = "encoding";

/// What a commit's drive reads: the index, the green and the locks as they stand for this request.
pub struct World<'a, G: ?Sized> {
  /// The index.
  pub index: &'a Index,
  /// The green.
  pub green: &'a G,
  /// The locks.
  pub locks: &'a LockTable,
}

/// The caller's answer to a hook due: passed, or its errors, outermost first.
pub type Hooks<'h> = &'h dyn Fn(&Due) -> Result<(), Vec<(u32, String)>>;

/// A PUT's text and headers.
#[derive(Clone, Copy, Debug)]
pub struct Put<'a> {
  /// The svndiff body.
  pub body: &'a [u8],
  /// `X-SVN-Base-Fulltext-MD5`.
  pub base_md5: Option<&'a str>,
  /// `X-SVN-Result-Fulltext-MD5`.
  pub result_md5: Option<&'a str>,
  /// `X-SVN-Version-Name`: an opened file's base revision.
  pub version_name: Option<u64>,
}

/// What a MERGE came to.
#[derive(Debug)]
pub enum Merged {
  /// The work the verdict accepted, with the sink holding its texts: the caller commits it, reading its texts from the
  /// sink, and answers with [`super::response::merge`].
  Accepted(Box<Work>, Box<dyn TextSink>),
  /// Refused: this is the answer.
  Refused(HttpAnswer),
}

/// How far a MERGE went before its caller's hook (A-733): stopped at pre-commit, which the caller runs and answers with
/// [`DavCommit::resume_merge`]; or ended.
#[derive(Debug)]
pub enum MergeStep {
  /// pre-commit is due.
  Hook(Due),
  /// The MERGE ended.
  Done(Merged),
}

/// One editor command's step as far as its hook (A-733): what it made, or the hook due, which the caller runs and
/// answers with the drive's resume.
enum EditStep {
  /// What it made.
  Done(Edited),
  /// A hook is due.
  Hook(Due),
}

/// The revision properties a POST's skel names (`create_txn.c`: `( create-txn-with-props ( NAME VALUE ... ) )` or
/// `( create-txn )`), each name once, `svn:author` and `svn:date` dropped (the principal's and the commit's own), then
/// `svn:author` set to `user` where one is named; refused where the skel is malformed.
///
/// # Errors
/// [`DavRefusal::Malformed`] for a body that is no such skel.
pub fn transaction_properties(
  body: &[u8],
  user: Option<&str>,
) -> Result<PropertyPairs, DavRefusal> {
  let malformed = || DavRefusal::Malformed {
    element: "create-txn",
    text: String::from_utf8_lossy(body).into_owned(),
  };
  let skel = Skel::parse(body).ok_or_else(malformed)?;
  let Skel::List(items) = skel else {
    return Err(malformed());
  };
  let mut properties = BTreeMap::new();
  match items.as_slice() {
    [Skel::Atom(word)] if word.as_slice() == b"create-txn" => {}
    [Skel::Atom(word), Skel::List(pairs)] if word.as_slice() == b"create-txn-with-props" => {
      for pair in pairs.chunks(2) {
        let [Skel::Atom(name), Skel::Atom(value)] = pair else {
          return Err(malformed());
        };
        properties.insert(name.clone(), value.clone());
      }
    }
    _ => return Err(malformed()),
  }
  properties.remove(REVISION_AUTHOR);
  properties.remove(REVISION_DATE);
  if let Some(user) = user {
    properties.insert(REVISION_AUTHOR.to_vec(), user.as_bytes().to_vec());
  }
  Ok(properties.into_iter().collect())
}

/// The POST's answer (`create_txn.c`): `201` naming the transaction.
pub fn created(transaction: &str) -> HttpAnswer {
  HttpAnswer {
    status: HTTP_CREATED,
    headers: vec![("SVN-Txn-Name", transaction.to_owned())],
    body: Vec::new(),
  }
}

/// The answer to a request on a transaction that has ended or never was (`mod_dav_svn`'s "No such transaction").
pub fn no_such_transaction(transaction: &str) -> HttpAnswer {
  error_answer(
    HTTP_NOT_FOUND,
    true,
    SVN_ERR_FS_NO_SUCH_TRANSACTION,
    &format!("No such transaction '{transaction}'"),
  )
}

/// One HTTPv2 transaction's drive (the module doc).
pub struct DavCommit {
  /// The commit drive.
  drive: Drive,
  /// The repository's root URL, which a copy's source is named beneath.
  repository_url: String,
  /// The directories open, by path from the root (`""` the root), each with its token.
  directories: BTreeMap<String, Vec<u8>>,
  /// The paths this transaction added, files and directories.
  added: BTreeSet<String>,
  /// The tokens handed out.
  tokens: u64,
  /// Whether the transaction has ended: refused, aborted or merged.
  over: bool,
  /// The PUT whose text is being taken, between its body's runs ([`DavCommit::begin_put`]).
  putting: Option<Putting>,
  /// Where its new texts go, a window at a time (§4.17.1 rule 1; [`TextSink`]): the caller's, handed back with the
  /// accepted work, dropped with a commit that ends otherwise.
  texts: Box<dyn TextSink>,
}

/// A PUT whose delta is open: its file's token, its path, whether it adds the file, and the result checksum its
/// close is judged by.
#[derive(Debug)]
struct Putting {
  /// The file's token.
  file: Vec<u8>,
  /// The editor's path.
  path: String,
  /// Whether this PUT adds the file.
  adds: bool,
  /// `X-SVN-Result-Fulltext-MD5`.
  result_md5: Option<String>,
}

/// A PUT's headers, which open its file before its body's first run (the module doc).
#[derive(Clone, Copy, Debug)]
pub struct PutStart<'a> {
  /// `X-SVN-Base-Fulltext-MD5`.
  pub base_md5: Option<&'a str>,
  /// `X-SVN-Result-Fulltext-MD5`.
  pub result_md5: Option<&'a str>,
  /// `X-SVN-Version-Name`: an opened file's base revision.
  pub version_name: Option<u64>,
}

impl std::fmt::Debug for DavCommit {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("DavCommit")
      .field("transaction", &self.drive.pending().transaction)
      .field("directories", &self.directories)
      .field("added", &self.added)
      .field("over", &self.over)
      .finish_non_exhaustive()
  }
}

impl DavCommit {
  /// Begins a transaction over `world` with `setup` (its revision properties from [`transaction_properties`]): the
  /// edit's root opened at the youngest, start-commit answered by `hooks`.
  ///
  /// # Errors
  /// The answer to the POST where the drive refused it.
  pub fn begin<G: Green + ?Sized>(
    setup: Setup,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    texts: Box<dyn TextSink>,
  ) -> Result<Self, HttpAnswer> {
    let (mut commit, due) = Self::begin_hooked(setup, world, texts)?;
    if let Some(due) = due {
      commit.resume_begin(world, hooks(&due))?;
    }
    Ok(commit)
  }

  /// Begins a transaction as [`DavCommit::begin`] does, as far as start-commit (A-733): the commit, and start-commit's
  /// due where the drive stopped for it, which the caller runs and answers with [`DavCommit::resume_begin`].
  ///
  /// # Errors
  /// The answer to the POST where the drive refused it.
  pub fn begin_hooked<G: Green + ?Sized>(
    setup: Setup,
    world: &World<'_, G>,
    texts: Box<dyn TextSink>,
  ) -> Result<(Self, Option<Due>), HttpAnswer> {
    let mut commit = Self {
      texts,
      repository_url: setup.repository_url.clone(),
      drive: Drive::new(setup),
      directories: BTreeMap::new(),
      added: BTreeSet::new(),
      tokens: 0,
      over: false,
      putting: None,
    };
    let opened = commit.edit_until_hook(
      world,
      &EditCommand::OpenRoot {
        revision: None,
        root: ROOT_TOKEN,
      },
    )?;
    match opened {
      EditStep::Hook(due) => Ok((commit, Some(due))),
      EditStep::Done(edited) => {
        commit.root_opened(edited)?;
        Ok((commit, None))
      }
    }
  }

  /// Answers the start-commit the begun transaction stopped for (A-733): the root opened, or the POST's refusal.
  ///
  /// # Errors
  /// The answer to the POST where the hook or the drive refused it.
  pub fn resume_begin<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    answer: Result<(), Vec<(u32, String)>>,
  ) -> Result<(), HttpAnswer> {
    match self.resume_until_hook(world, answer)? {
      EditStep::Done(edited) => self.root_opened(edited),
      // `open-root` meets one hook alone: a second is no step this drive takes.
      EditStep::Hook(_) => {
        self.over = true;
        Err(no_such_transaction(self.drive.pending().transaction))
      }
    }
  }

  /// The root's `open-root` made `edited`: the root kept open until the MERGE, or the transaction ended.
  fn root_opened(&mut self, edited: Edited) -> Result<(), HttpAnswer> {
    if edited != Edited::Taken {
      self.over = true;
      return Err(no_such_transaction(self.drive.pending().transaction));
    }
    self.directories.insert(String::new(), ROOT_TOKEN.to_vec());
    Ok(())
  }

  /// Whether the transaction has ended.
  pub fn over(&self) -> bool {
    self.over
  }

  /// The revision the transaction was begun on: what its hooks' pending view names (A-733).
  pub fn pending_base(&self) -> Revision {
    self.drive.pending().base
  }

  /// The lock tokens the transaction presented, each by its repository path, as `mod_dav_svn` hands them to the
  /// filesystem (`svn_fs_access_add_lock_token2`) and so to pre-commit's standard input (A-733).
  pub fn pending_tokens(&self) -> Vec<(String, String)> {
    self
      .drive
      .pending()
      .presented
      .iter()
      .map(|(path, token)| (path.clone(), token.clone()))
      .collect()
  }

  /// Presents a lock's `token` for the transaction path `path` (an `If` header's, or a MERGE's `S:lock-token-list`,
  /// A-681): a later request meeting the path's lock, and the MERGE's verdict, judge it with this token.
  pub fn present(&mut self, path: &str, token: &str) {
    self
      .drive
      .present_lock(format!("/{}", relpath(path)), token.to_owned());
  }

  /// Whether the commit keeps the locks it presented (`X-SVN-Options: keep-locks`), as the MERGE asks.
  pub fn keep_locks(&mut self, keep: bool) {
    self.drive.keep_locks(keep);
  }

  /// The tokens an `If` header presents (RFC 4918 §10.4): each state token in angle brackets that is a lock token
  /// (`opaquelocktoken:`, as Subversion's are), a resource tag's URL passed over.
  pub fn if_tokens(header: &str) -> Vec<&str> {
    header
      .split('<')
      .filter_map(|rest| rest.split_once('>').map(|(inside, _)| inside))
      .filter(|inside| inside.starts_with(LOCK_TOKEN_SCHEME))
      .collect()
  }

  /// A PUT of `path`'s text (the module doc): `201` for a file this request adds, `204` for one it changes.
  pub fn put<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    path: &str,
    put: &Put<'_>,
  ) -> HttpAnswer {
    self
      .put_file(world, hooks, &relpath(path), put)
      .unwrap_or_else(|refused| refused)
  }

  /// A MKCOL of `path`: the directory added, open until the MERGE; `201`.
  pub fn mkcol<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    path: &str,
  ) -> HttpAnswer {
    let path = relpath(path);
    self
      .add_directory(world, hooks, &path, None)
      .map(|()| bare(HTTP_CREATED))
      .unwrap_or_else(|refused| refused)
  }

  /// A DELETE of `path` at `revision` (`X-SVN-Version-Name`); `204`.
  pub fn delete<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    path: &str,
    revision: Option<u64>,
  ) -> HttpAnswer {
    let path = relpath(path);
    let deleted = self.open_parents(world, hooks, &path).and_then(|parent| {
      self.step(
        world,
        hooks,
        &EditCommand::DeleteEntry {
          path: path.as_bytes(),
          revision,
          directory: &parent,
        },
      )
    });
    match deleted {
      Ok(()) => {
        self.added.remove(&path);
        bare(HTTP_NO_CONTENT)
      }
      Err(refused) => refused,
    }
  }

  /// A COPY of the repository path `source` in `source_revision` to the transaction path `destination`: a directory
  /// added copied and left open, a file added copied and closed; `201`.
  pub fn copy<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    (source, source_revision): (&str, Revision),
    destination: &str,
  ) -> HttpAnswer {
    let destination = relpath(destination);
    let kind = match world.index.node(source, source_revision) {
      Ok(Some(node)) => node.kind(),
      _ => {
        return error_answer(
          HTTP_NOT_FOUND,
          true,
          SVN_ERR_FS_NOT_FOUND,
          &format!("File not found: revision {source_revision}, path '{source}'"),
        );
      }
    };
    let url = format!("{}{source}", self.repository_url.trim_end_matches('/'));
    let copied_from = CopiedFrom {
      path: url.as_bytes(),
      revision: source_revision,
    };
    let copied = match kind {
      NodeKind::Directory => self.add_directory(world, hooks, &destination, Some(copied_from)),
      _ => self.add_copied_file(world, hooks, &destination, copied_from),
    };
    copied
      .map(|()| bare(HTTP_CREATED))
      .unwrap_or_else(|refused| refused)
  }

  /// A PROPPATCH of `path` with `body` read within `bounds`: each property set or removed on the node, `207` with
  /// each property's `200` (`mod_dav`'s answer to a PROPPATCH that succeeds whole).
  pub fn proppatch<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    (path, revision): (&str, Option<u64>),
    (body, bounds): (&[u8], &Bounds),
  ) -> HttpAnswer {
    let changes = match property_changes(body, bounds) {
      Ok(changes) => changes,
      Err(_) => {
        return error_answer(
          HTTP_BAD_REQUEST,
          true,
          SVN_ERR_DAV_MALFORMED_DATA,
          "The PROPPATCH body could not be read",
        );
      }
    };
    let path = relpath(path);
    let changed = self.change_properties(world, hooks, (&path, revision), &changes);
    match changed {
      Ok(()) => proppatched(&changes),
      Err(refused) => refused,
    }
  }

  /// The MERGE: every directory closed, deepest first, then `close-edit`, pre-commit answered by `hooks`.
  pub fn merge<G: Green + ?Sized>(&mut self, world: &World<'_, G>, hooks: Hooks<'_>) -> Merged {
    match self.merge_hooked(world) {
      MergeStep::Done(merged) => merged,
      MergeStep::Hook(due) => {
        let answer = hooks(&due);
        self.resume_merge(world, answer)
      }
    }
  }

  /// The MERGE as [`DavCommit::merge`] makes it, as far as pre-commit (A-733): its outcome, or pre-commit's due, which
  /// the caller runs and answers with [`DavCommit::resume_merge`].
  pub fn merge_hooked<G: Green + ?Sized>(&mut self, world: &World<'_, G>) -> MergeStep {
    let mut open: Vec<(String, Vec<u8>)> =
      std::mem::take(&mut self.directories).into_iter().collect();
    open.sort_by_key(|(path, _)| {
      std::cmp::Reverse(
        path
          .matches('/')
          .count()
          .saturating_add(usize::from(!path.is_empty())),
      )
    });
    for (_, token) in &open {
      match self.edit_until_hook(world, &EditCommand::CloseDir { directory: token }) {
        Ok(EditStep::Done(Edited::Taken)) => {}
        // `close-dir` meets no hook: anything else ends the transaction.
        Ok(_) => {
          self.over = true;
          return MergeStep::Done(Merged::Refused(no_such_transaction(
            self.drive.pending().transaction,
          )));
        }
        Err(refused) => return MergeStep::Done(Merged::Refused(refused)),
      }
    }
    match self.edit_until_hook(world, &EditCommand::CloseEdit) {
      Ok(EditStep::Hook(due)) => MergeStep::Hook(due),
      Ok(EditStep::Done(edited)) => MergeStep::Done(self.merged(Ok(edited))),
      Err(refused) => MergeStep::Done(Merged::Refused(refused)),
    }
  }

  /// Answers the pre-commit the MERGE stopped for (A-733): the work accepted, or the MERGE's refusal.
  pub fn resume_merge<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    answer: Result<(), Vec<(u32, String)>>,
  ) -> Merged {
    match self.resume_until_hook(world, answer) {
      Ok(EditStep::Done(edited)) => self.merged(Ok(edited)),
      // `close-edit` meets pre-commit alone: a second hook is no step this drive takes.
      Ok(EditStep::Hook(_)) => self.merged(Ok(Edited::Closed)),
      Err(refused) => Merged::Refused(refused),
    }
  }

  /// What `close-edit` made of the transaction.
  fn merged(&mut self, edited: Result<Edited, HttpAnswer>) -> Merged {
    match edited {
      Ok(Edited::Accepted(work)) => {
        self.over = true;
        let texts = std::mem::replace(
          &mut self.texts,
          Box::new(crate::commit::drive::HeldTexts::new()),
        );
        Merged::Accepted(work, texts)
      }
      Ok(_) => {
        self.over = true;
        Merged::Refused(error_answer(
          HTTP_INTERNAL_SERVER_ERROR,
          true,
          SVN_ERR_FS_NO_SUCH_TRANSACTION,
          "The commit did not reach a verdict",
        ))
      }
      Err(refused) => Merged::Refused(refused),
    }
  }

  /// A DELETE of the transaction: `abort-edit`; `204`.
  pub fn abort<G: Green + ?Sized>(&mut self, world: &World<'_, G>, hooks: Hooks<'_>) -> HttpAnswer {
    if !self.over {
      let _aborted = self.edit(world, hooks, &EditCommand::AbortEdit);
    }
    self.over = true;
    bare(HTTP_NO_CONTENT)
  }

  /// A PUT's file: begun, its whole body one chunk, ended.
  fn put_file<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    path: &str,
    put: &Put<'_>,
  ) -> Result<HttpAnswer, HttpAnswer> {
    let start = PutStart {
      base_md5: put.base_md5,
      result_md5: put.result_md5,
      version_name: put.version_name,
    };
    self.begin_put(world, hooks, path, &start)?;
    self.put_chunk(world, hooks, put.body)?;
    Ok(self.end_put(world, hooks))
  }

  /// Begins a PUT of `path`'s text before its body's first run (§4.17.1 "A push's intake": a body is taken as it
  /// arrives, never held whole): its parents opened, the file opened at the request's base revision or added, its delta
  /// opened with the base checksum.
  ///
  /// # Errors
  /// The answer to the PUT where the drive refused it, which ends the transaction.
  pub fn begin_put<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    path: &str,
    start: &PutStart<'_>,
  ) -> Result<(), HttpAnswer> {
    let path = relpath(path);
    let parent = self.open_parents(world, hooks, &path)?;
    let file = self.token(b'c');
    let adds = start.version_name.is_none() && !self.added.contains(&path);
    let command = if adds {
      EditCommand::AddFile {
        path: path.as_bytes(),
        directory: &parent,
        file: &file,
        copied_from: None,
      }
    } else {
      EditCommand::OpenFile {
        path: path.as_bytes(),
        directory: &parent,
        file: &file,
        revision: start.version_name,
      }
    };
    self.step(world, hooks, &command)?;
    self.step(
      world,
      hooks,
      &EditCommand::ApplyTextdelta {
        file: &file,
        base_checksum: start.base_md5.map(str::as_bytes),
      },
    )?;
    self.putting = Some(Putting {
      file,
      path,
      adds,
      result_md5: start.result_md5.map(str::to_owned),
    });
    Ok(())
  }

  /// One run of the PUT's body, handed to the drive as the delta's next chunk and held no longer: the drive applies
  /// each window as it completes.
  ///
  /// # Errors
  /// The answer to the PUT where the drive refused the chunk, or where no PUT was begun.
  pub fn put_chunk<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    chunk: &[u8],
  ) -> Result<(), HttpAnswer> {
    let Some(file) = self.putting.as_ref().map(|putting| putting.file.clone()) else {
      return Err(unserved("A PUT's body with no PUT begun"));
    };
    self.step(
      world,
      hooks,
      &EditCommand::TextdeltaChunk { file: &file, chunk },
    )
  }

  /// Ends the PUT begun last: its delta ended and its file closed with the result checksum; `201` for a file it added,
  /// `204` for one it changed.
  pub fn end_put<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
  ) -> HttpAnswer {
    let Some(putting) = self.putting.take() else {
      return unserved("A PUT's end with no PUT begun");
    };
    let ended = self
      .step(
        world,
        hooks,
        &EditCommand::TextdeltaEnd {
          file: &putting.file,
        },
      )
      .and_then(|()| {
        self.step(
          world,
          hooks,
          &EditCommand::CloseFile {
            file: &putting.file,
            text_checksum: putting.result_md5.as_deref().map(str::as_bytes),
          },
        )
      });
    match ended {
      Ok(()) if putting.adds => {
        self.added.insert(putting.path);
        bare(HTTP_CREATED)
      }
      Ok(()) => bare(HTTP_NO_CONTENT),
      Err(refused) => refused,
    }
  }

  /// A directory added at `path`, copied from `copied_from` where one is named, and left open.
  fn add_directory<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    path: &str,
    copied_from: Option<CopiedFrom<'_>>,
  ) -> Result<(), HttpAnswer> {
    let parent = self.open_parents(world, hooks, path)?;
    let child = self.token(b'd');
    self.step(
      world,
      hooks,
      &EditCommand::AddDir {
        path: path.as_bytes(),
        parent: &parent,
        child: &child,
        copied_from,
      },
    )?;
    self.directories.insert(path.to_owned(), child);
    self.added.insert(path.to_owned());
    Ok(())
  }

  /// A file added at `path`, copied from `copied_from`, and closed.
  fn add_copied_file<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    path: &str,
    copied_from: CopiedFrom<'_>,
  ) -> Result<(), HttpAnswer> {
    let parent = self.open_parents(world, hooks, path)?;
    let file = self.token(b'c');
    self.step(
      world,
      hooks,
      &EditCommand::AddFile {
        path: path.as_bytes(),
        directory: &parent,
        file: &file,
        copied_from: Some(copied_from),
      },
    )?;
    self.step(
      world,
      hooks,
      &EditCommand::CloseFile {
        file: &file,
        text_checksum: None,
      },
    )?;
    self.added.insert(path.to_owned());
    Ok(())
  }

  /// Each of `changes` made on the node at `path`: a directory's on its open token, a file's on it opened for them.
  fn change_properties<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    (path, revision): (&str, Option<u64>),
    changes: &[(Vec<u8>, Option<Vec<u8>>)],
  ) -> Result<(), HttpAnswer> {
    let directory = self.directories.contains_key(path) || self.is_directory(world, path);
    if directory {
      let token = self.open_directory(world, hooks, path, revision)?;
      for (name, value) in changes {
        self.step(
          world,
          hooks,
          &EditCommand::ChangeDirProp {
            directory: &token,
            name,
            value: value.as_deref(),
          },
        )?;
      }
      return Ok(());
    }
    let parent = self.open_parents(world, hooks, path)?;
    let file = self.token(b'c');
    let revision = if self.added.contains(path) {
      None
    } else {
      revision
    };
    self.step(
      world,
      hooks,
      &EditCommand::OpenFile {
        path: path.as_bytes(),
        directory: &parent,
        file: &file,
        revision,
      },
    )?;
    for (name, value) in changes {
      self.step(
        world,
        hooks,
        &EditCommand::ChangeFileProp {
          file: &file,
          name,
          value: value.as_deref(),
        },
      )?;
    }
    self.step(
      world,
      hooks,
      &EditCommand::CloseFile {
        file: &file,
        text_checksum: None,
      },
    )
  }

  /// Whether `path` is a directory in the transaction's base: one this transaction added is in [`Self::directories`].
  fn is_directory<G: ?Sized>(&self, world: &World<'_, G>, path: &str) -> bool {
    let base = self.drive.pending().base;
    matches!(
      world.index.node(&format!("/{path}"), base),
      Ok(Some(node)) if node.kind() == NodeKind::Directory
    )
  }

  /// The token of the directory at `path`, opened (with its parents) where it is not yet.
  fn open_directory<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    path: &str,
    revision: Option<u64>,
  ) -> Result<Vec<u8>, HttpAnswer> {
    if let Some(token) = self.directories.get(path) {
      return Ok(token.clone());
    }
    let parent = self.open_parents(world, hooks, path)?;
    let child = self.token(b'd');
    self.step(
      world,
      hooks,
      &EditCommand::OpenDir {
        path: path.as_bytes(),
        parent: &parent,
        child: &child,
        revision,
      },
    )?;
    self.directories.insert(path.to_owned(), child.clone());
    Ok(child)
  }

  /// The token of `path`'s parent directory, every directory from the root down to it opened where it is not yet.
  fn open_parents<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    path: &str,
  ) -> Result<Vec<u8>, HttpAnswer> {
    let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
    if parent.is_empty() {
      return Ok(ROOT_TOKEN.to_vec());
    }
    self.open_directory(world, hooks, parent, None)
  }

  /// A fresh token, `kind` its first byte (`d` a directory's, `c` a file's, as ra_svn's client names them).
  fn token(&mut self, kind: u8) -> Vec<u8> {
    self.tokens = self.tokens.saturating_add(1);
    let mut token = vec![kind];
    token.extend_from_slice(self.tokens.to_string().as_bytes());
    token
  }

  /// One command that must be taken.
  fn step<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    command: &EditCommand<'_>,
  ) -> Result<(), HttpAnswer> {
    match self.edit(world, hooks, command)? {
      Edited::Taken => Ok(()),
      _ => {
        self.over = true;
        Err(no_such_transaction(self.drive.pending().transaction))
      }
    }
  }

  /// One command, every hook it meets answered by `hooks`: what it made, or its refusal's answer, which ends the
  /// transaction.
  fn edit<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    hooks: Hooks<'_>,
    command: &EditCommand<'_>,
  ) -> Result<Edited, HttpAnswer> {
    let mut step = self.edit_until_hook(world, command)?;
    loop {
      match step {
        EditStep::Done(edited) => return Ok(edited),
        EditStep::Hook(due) => step = self.resume_until_hook(world, hooks(&due))?,
      }
    }
  }

  /// One command as far as its hook (A-733): what it made, the hook due, or its refusal's answer, which ends the
  /// transaction.
  fn edit_until_hook<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    command: &EditCommand<'_>,
  ) -> Result<EditStep, HttpAnswer> {
    if self.over {
      return Err(no_such_transaction(self.drive.pending().transaction));
    }
    let edited = self
      .drive
      .edit(world.index, world.green, world.locks, &*self.texts, command);
    self.edit_step(edited)
  }

  /// The drive resumed with a hook's `answer` (A-733): what the command made, another hook due, or its refusal's answer.
  fn resume_until_hook<G: Green + ?Sized>(
    &mut self,
    world: &World<'_, G>,
    answer: Result<(), Vec<(u32, String)>>,
  ) -> Result<EditStep, HttpAnswer> {
    if self.over {
      return Err(no_such_transaction(self.drive.pending().transaction));
    }
    let edited =
      self
        .drive
        .resume_edit(world.index, world.green, world.locks, &*self.texts, answer);
    self.edit_step(edited)
  }

  /// What the drive's `edited` makes of the transaction: a refusal or a close ends it.
  fn edit_step(&mut self, edited: Edited) -> Result<EditStep, HttpAnswer> {
    match edited {
      Edited::Hook(due) => Ok(EditStep::Hook(due)),
      Edited::Refused(errors) => {
        self.over = true;
        Err(refused(&errors))
      }
      Edited::Closed => {
        self.over = true;
        Err(no_such_transaction(self.drive.pending().transaction))
      }
      other => Ok(EditStep::Done(other)),
    }
  }
}

/// A transaction path (`/trunk/a.txt`) as the editor's path from the root (`trunk/a.txt`).
fn relpath(path: &str) -> String {
  path
    .split('/')
    .filter(|name| !name.is_empty())
    .collect::<Vec<_>>()
    .join("/")
}

/// An answer with `status` and nothing else.
fn bare(status: u16) -> HttpAnswer {
  HttpAnswer {
    status,
    headers: Vec::new(),
    body: Vec::new(),
  }
}

/// The answer to a Subversion error `code` with `message` (`dav_svn__convert_err`: its status from the error, `500`
/// where the error names none).
pub fn refusal(code: u32, message: &str) -> HttpAnswer {
  error_answer(
    convert_status(code, HTTP_INTERNAL_SERVER_ERROR),
    true,
    code,
    message,
  )
}

/// The answer to a refused command: its outermost error, with `dav_svn__convert_err`'s status.
fn refused(errors: &[(u32, String)]) -> HttpAnswer {
  let (code, message) = errors.first().cloned().unwrap_or((
    SVN_ERR_UNSUPPORTED_FEATURE,
    String::from("The commit was refused"),
  ));
  error_answer(
    convert_status(code, HTTP_INTERNAL_SERVER_ERROR),
    true,
    code,
    &message,
  )
}

/// The answer to a request on a transaction resource this site does not serve.
pub fn unserved(what: &str) -> HttpAnswer {
  error_answer(
    HTTP_NOT_IMPLEMENTED,
    true,
    SVN_ERR_UNSUPPORTED_FEATURE,
    &format!("{what} is not served here"),
  )
}

/// Property changes in order: each name, and its new value or none for a removal.
type PropertyChanges = Vec<(Vec<u8>, Option<Vec<u8>>)>;

/// A PROPPATCH body's changes (`mod_dav.c` `dav_method_proppatch`): each `D:set` property with its value (base64
/// where `V:encoding` says so) and each `D:remove` property with none, in order, named as Subversion names them (its
/// own namespace `svn:`, the custom namespace bare).
fn property_changes(body: &[u8], bounds: &Bounds) -> Result<PropertyChanges, DavRefusal> {
  let malformed = || DavRefusal::Malformed {
    element: "propertyupdate",
    text: String::new(),
  };
  let root = parse(body, bounds).map_err(DavRefusal::Xml)?;
  if !root.is(DAV, "propertyupdate") {
    return Err(malformed());
  }
  let mut changes = Vec::new();
  for action in root.elements() {
    let set = action.is(DAV, "set");
    if !set && !action.is(DAV, "remove") {
      continue;
    }
    for prop in action.elements().filter(|element| element.is(DAV, "prop")) {
      for property in prop.elements() {
        let name = property_name(property).ok_or_else(malformed)?;
        let value = if set {
          Some(property_value(property).ok_or_else(malformed)?)
        } else {
          None
        };
        changes.push((name, value));
      }
    }
  }
  Ok(changes)
}

/// A property element's Subversion name; none for a namespace Subversion does not keep properties in.
fn property_name(property: &Element) -> Option<Vec<u8>> {
  match property.namespace.as_str() {
    SVN_PROPS => Some(format!("{SVN_PREFIX}{}", property.name).into_bytes()),
    CUSTOM_PROPS => Some(property.name.clone().into_bytes()),
    _ => None,
  }
}

/// A set property's value: its text, decoded from base64 where its `encoding` attribute says so.
fn property_value(property: &Element) -> Option<Vec<u8>> {
  let text = property.text();
  match property.attribute(ENCODING) {
    Some("base64") => base64_decode(&text),
    Some(_) => None,
    None => Some(text.into_bytes()),
  }
}

/// `207` with each changed property's `200`, as `mod_dav` answers a PROPPATCH that succeeds whole.
fn proppatched(changes: &[(Vec<u8>, Option<Vec<u8>>)]) -> HttpAnswer {
  let mut body = String::from(
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\" xmlns:ns1=\"",
  );
  body.push_str(SVN_PROPS);
  body.push_str("\" xmlns:ns2=\"");
  body.push_str(CUSTOM_PROPS);
  body.push_str("\" xmlns:V=\"");
  body.push_str(SVN_DAV);
  body.push_str("\">\n<D:response>\n<D:propstat>\n<D:prop>\n");
  for (name, _) in changes {
    let name = String::from_utf8_lossy(name);
    match name.strip_prefix(SVN_PREFIX) {
      Some(local) => body.push_str(&format!("<ns1:{local}/>\n")),
      None => body.push_str(&format!("<ns2:{name}/>\n")),
    }
  }
  body.push_str("</D:prop>\n<D:status>HTTP/1.1 200 OK</D:status>\n</D:propstat>\n</D:response>\n</D:multistatus>\n");
  HttpAnswer {
    status: HTTP_MULTI_STATUS,
    headers: vec![("Content-Type", String::from("text/xml; charset=\"utf-8\""))],
    body: body.into_bytes(),
  }
}

/// Format: base64's alphabet, each letter's value its place (RFC 4648 §4, Table 1).
const BASE64_ALPHABET: &[u8; 64] =
  b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
/// Format: the bits one base64 letter carries (RFC 4648 §4).
const BASE64_BITS: u32 = 6;

/// `text` decoded from base64 (RFC 4648 §4, whitespace skipped, as `svn_base64_decode_string` reads it); none where
/// it holds a byte outside the alphabet.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
  let mut out = Vec::new();
  let mut buffer = 0u32;
  let mut held = 0u32;
  for byte in text.bytes() {
    if matches!(byte, b'=' | b' ' | b'\t' | b'\r' | b'\n') {
      continue;
    }
    let value = BASE64_ALPHABET.iter().position(|letter| *letter == byte)?;
    buffer = (buffer << BASE64_BITS) | u32::try_from(value).ok()?;
    held = held.saturating_add(BASE64_BITS);
    if held >= u8::BITS {
      held = held.saturating_sub(u8::BITS);
      out.push(u8::try_from((buffer >> held) & u32::from(u8::MAX)).ok()?);
    }
  }
  Some(out)
}

/// A skel (`libsvn_subr/skel.c`): an atom, or a list of skels.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Skel {
  /// An atom's bytes.
  Atom(Vec<u8>),
  /// A list.
  List(Vec<Skel>),
}

impl Skel {
  /// The one skel `bytes` hold, whitespace around it allowed; none where they hold anything else.
  fn parse(bytes: &[u8]) -> Option<Self> {
    let (skel, rest) = Self::one(bytes)?;
    rest.iter().all(|byte| is_space(*byte)).then_some(skel)
  }

  /// The skel at the start of `bytes` (whitespace skipped) and what follows it.
  fn one(bytes: &[u8]) -> Option<(Self, &[u8])> {
    let at = bytes.iter().position(|byte| !is_space(*byte))?;
    let bytes = bytes.get(at..)?;
    match bytes.first()? {
      b'(' => Self::list(bytes.get(1..)?),
      first if first.is_ascii_digit() => Self::explicit_atom(bytes),
      _ => Self::implicit_atom(bytes),
    }
  }

  /// The list whose items start `rest`, just after its `(`, and what follows its `)`.
  fn list(mut rest: &[u8]) -> Option<(Self, &[u8])> {
    let mut items = Vec::new();
    loop {
      let at = rest.iter().position(|byte| !is_space(*byte))?;
      rest = rest.get(at..)?;
      if rest.first() == Some(&b')') {
        return Some((Self::List(items), rest.get(1..)?));
      }
      let (item, after) = Self::one(rest)?;
      items.push(item);
      rest = after;
    }
  }

  /// The explicit-length atom at the start of `bytes` (its length in digits, a space, then its
  /// bytes) and what follows it.
  fn explicit_atom(bytes: &[u8]) -> Option<(Self, &[u8])> {
    let digits = bytes
      .iter()
      .take_while(|byte| byte.is_ascii_digit())
      .count();
    let length: usize = std::str::from_utf8(bytes.get(..digits)?)
      .ok()?
      .parse()
      .ok()?;
    let start = digits.checked_add(1)?;
    if !is_space(*bytes.get(digits)?) {
      return None;
    }
    let end = start.checked_add(length)?;
    Some((
      Self::Atom(bytes.get(start..end)?.to_vec()),
      bytes.get(end..)?,
    ))
  }

  /// The implicit-length atom at the start of `bytes` (up to a space or a parenthesis) and what
  /// follows it.
  fn implicit_atom(bytes: &[u8]) -> Option<(Self, &[u8])> {
    let length = bytes
      .iter()
      .take_while(|byte| !is_space(**byte) && **byte != b'(' && **byte != b')')
      .count();
    Some((
      Self::Atom(bytes.get(..length)?.to_vec()),
      bytes.get(length..)?,
    ))
  }
}

/// Whether `byte` separates skels (`skel.c`'s space class).
fn is_space(byte: u8) -> bool {
  matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c')
}

#[cfg(test)]
mod tests;
