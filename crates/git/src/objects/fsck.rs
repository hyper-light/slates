//! git's fsck message ids and their severities (A-590; `fsck.h`'s `FOREACH_FSCK_MSG_ID` and `enum fsck_msg_type`), and
//! a check's [`Report`]: what git reports of one object, in its order, each finding a typed refusal or a typed warning.
//!
//! git's `fsck_vreport` (fsck.c) reports a fatal finding as an error and an info as a warning, and says nothing of an
//! ignored one; under `--strict` (and in a push git checks, see the module above) a warning is an error. A commit's and
//! a tag's check stops at its first error (each `if (err) return err`); a tree's reports everything it raised. A
//! [`Report`] keeps every finding, ignored ones too, so a policy that raises one (as `fsck.<id>=error` does) has it to
//! read.

/// A message's severity (fsck.h's `enum fsck_msg_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Severity {
  /// Stops the check at once; reported as an error.
  Fatal,
  /// Refuses the object.
  Error,
  /// Reported; the object is kept.
  Warn,
  /// "reported as warnings, but ignored by default" (fsck.h): reported as a warning.
  Info,
  /// "ignored (elevated when requested)" (fsck.h): not reported.
  Ignore,
}

/// Which of fsck's two strictnesses a check runs under (`fsck_options.strict`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strictness {
  /// `git fsck`'s: a warning is a warning, and a tree entry's mode 100664 is accepted.
  Default,
  /// `git fsck --strict`'s, and a push's that git checks (`index-pack --strict`): a warning refuses, and mode 100664
  /// is a bad mode.
  Strict,
}

impl Severity {
  /// The level a finding of this severity is reported at under `strictness` (fsck.c's `fsck_msg_type` and
  /// `fsck_vreport`): an error, a warning, or, for an ignored one, nothing ([`Severity::Ignore`]).
  pub const fn reported(self, strictness: Strictness) -> Severity {
    match (self, strictness) {
      (Severity::Fatal, _) | (Severity::Error, _) | (Severity::Warn, Strictness::Strict) => {
        Severity::Error
      }
      (Severity::Warn, Strictness::Default) | (Severity::Info, _) => Severity::Warn,
      (Severity::Ignore, _) => Severity::Ignore,
    }
  }
}

/// fsck.h's message ids (`FOREACH_FSCK_MSG_ID`), in its order, each with its default severity ([`FsckId::severity`])
/// and the name git prints ([`FsckId::name`]). The tree, commit and tag checks here report the ones `fsck_tree`,
/// `fsck_commit`, `fsck_tag_standalone` and `fsck_ident` report; the others are a ref's, a packed-refs file's and a
/// `.gitmodules` or `.gitattributes` blob's, kept so each of git's checks has its one typed name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FsckId {
  /// `NUL_IN_HEADER` (fatal), printed `nulInHeader`.
  NulInHeader,
  /// `UNTERMINATED_HEADER` (fatal), printed `unterminatedHeader`.
  UnterminatedHeader,
  /// `BAD_DATE` (error), printed `badDate`.
  BadDate,
  /// `BAD_DATE_OVERFLOW` (error), printed `badDateOverflow`.
  BadDateOverflow,
  /// `BAD_EMAIL` (error), printed `badEmail`.
  BadEmail,
  /// `BAD_NAME` (error), printed `badName`.
  BadName,
  /// `BAD_OBJECT_SHA1` (error), printed `badObjectSha1`.
  BadObjectSha1,
  /// `BAD_PACKED_REF_ENTRY` (error), printed `badPackedRefEntry`.
  BadPackedRefEntry,
  /// `BAD_PACKED_REF_HEADER` (error), printed `badPackedRefHeader`.
  BadPackedRefHeader,
  /// `BAD_PARENT_SHA1` (error), printed `badParentSha1`.
  BadParentSha1,
  /// `BAD_REF_CONTENT` (error), printed `badRefContent`.
  BadRefContent,
  /// `BAD_REF_FILETYPE` (error), printed `badRefFiletype`.
  BadRefFiletype,
  /// `BAD_REF_NAME` (error), printed `badRefName`.
  BadRefName,
  /// `BAD_REFERENT_NAME` (error), printed `badReferentName`.
  BadReferentName,
  /// `BAD_TIMEZONE` (error), printed `badTimezone`.
  BadTimezone,
  /// `BAD_TREE` (error), printed `badTree`.
  BadTree,
  /// `BAD_TREE_SHA1` (error), printed `badTreeSha1`.
  BadTreeSha1,
  /// `BAD_TYPE` (error), printed `badType`.
  BadType,
  /// `DUPLICATE_ENTRIES` (error), printed `duplicateEntries`.
  DuplicateEntries,
  /// `MISSING_AUTHOR` (error), printed `missingAuthor`.
  MissingAuthor,
  /// `MISSING_COMMITTER` (error), printed `missingCommitter`.
  MissingCommitter,
  /// `MISSING_EMAIL` (error), printed `missingEmail`.
  MissingEmail,
  /// `MISSING_NAME_BEFORE_EMAIL` (error), printed `missingNameBeforeEmail`.
  MissingNameBeforeEmail,
  /// `MISSING_OBJECT` (error), printed `missingObject`.
  MissingObject,
  /// `MISSING_SPACE_BEFORE_DATE` (error), printed `missingSpaceBeforeDate`.
  MissingSpaceBeforeDate,
  /// `MISSING_SPACE_BEFORE_EMAIL` (error), printed `missingSpaceBeforeEmail`.
  MissingSpaceBeforeEmail,
  /// `MISSING_TAG` (error), printed `missingTag`.
  MissingTag,
  /// `MISSING_TAG_ENTRY` (error), printed `missingTagEntry`.
  MissingTagEntry,
  /// `MISSING_TREE` (error), printed `missingTree`.
  MissingTree,
  /// `MISSING_TYPE` (error), printed `missingType`.
  MissingType,
  /// `MISSING_TYPE_ENTRY` (error), printed `missingTypeEntry`.
  MissingTypeEntry,
  /// `MULTIPLE_AUTHORS` (error), printed `multipleAuthors`.
  MultipleAuthors,
  /// `PACKED_REF_ENTRY_NOT_TERMINATED` (error), printed `packedRefEntryNotTerminated`.
  PackedRefEntryNotTerminated,
  /// `PACKED_REF_UNSORTED` (error), printed `packedRefUnsorted`.
  PackedRefUnsorted,
  /// `TREE_NOT_SORTED` (error), printed `treeNotSorted`.
  TreeNotSorted,
  /// `UNKNOWN_TYPE` (error), printed `unknownType`.
  UnknownType,
  /// `ZERO_PADDED_DATE` (error), printed `zeroPaddedDate`.
  ZeroPaddedDate,
  /// `GITMODULES_MISSING` (error), printed `gitmodulesMissing`.
  GitmodulesMissing,
  /// `GITMODULES_BLOB` (error), printed `gitmodulesBlob`.
  GitmodulesBlob,
  /// `GITMODULES_LARGE` (error), printed `gitmodulesLarge`.
  GitmodulesLarge,
  /// `GITMODULES_NAME` (error), printed `gitmodulesName`.
  GitmodulesName,
  /// `GITMODULES_SYMLINK` (error), printed `gitmodulesSymlink`.
  GitmodulesSymlink,
  /// `GITMODULES_URL` (error), printed `gitmodulesUrl`.
  GitmodulesUrl,
  /// `GITMODULES_PATH` (error), printed `gitmodulesPath`.
  GitmodulesPath,
  /// `GITMODULES_UPDATE` (error), printed `gitmodulesUpdate`.
  GitmodulesUpdate,
  /// `GITATTRIBUTES_MISSING` (error), printed `gitattributesMissing`.
  GitattributesMissing,
  /// `GITATTRIBUTES_LARGE` (error), printed `gitattributesLarge`.
  GitattributesLarge,
  /// `GITATTRIBUTES_LINE_LENGTH` (error), printed `gitattributesLineLength`.
  GitattributesLineLength,
  /// `GITATTRIBUTES_BLOB` (error), printed `gitattributesBlob`.
  GitattributesBlob,
  /// `EMPTY_NAME` (warn), printed `emptyName`.
  EmptyName,
  /// `FULL_PATHNAME` (warn), printed `fullPathname`.
  FullPathname,
  /// `HAS_DOT` (warn), printed `hasDot`.
  HasDot,
  /// `HAS_DOTDOT` (warn), printed `hasDotdot`.
  HasDotdot,
  /// `HAS_DOTGIT` (warn), printed `hasDotgit`.
  HasDotgit,
  /// `NULL_SHA1` (warn), printed `nullSha1`.
  NullSha1,
  /// `ZERO_PADDED_FILEMODE` (warn), printed `zeroPaddedFilemode`.
  ZeroPaddedFilemode,
  /// `NUL_IN_COMMIT` (warn), printed `nulInCommit`.
  NulInCommit,
  /// `LARGE_PATHNAME` (warn), printed `largePathname`.
  LargePathname,
  /// `BAD_FILEMODE` (info), printed `badFilemode`.
  BadFilemode,
  /// `EMPTY_PACKED_REFS_FILE` (info), printed `emptyPackedRefsFile`.
  EmptyPackedRefsFile,
  /// `GITMODULES_PARSE` (info), printed `gitmodulesParse`.
  GitmodulesParse,
  /// `GITIGNORE_SYMLINK` (info), printed `gitignoreSymlink`.
  GitignoreSymlink,
  /// `GITATTRIBUTES_SYMLINK` (info), printed `gitattributesSymlink`.
  GitattributesSymlink,
  /// `MAILMAP_SYMLINK` (info), printed `mailmapSymlink`.
  MailmapSymlink,
  /// `BAD_TAG_NAME` (info), printed `badTagName`.
  BadTagName,
  /// `MISSING_TAGGER_ENTRY` (info), printed `missingTaggerEntry`.
  MissingTaggerEntry,
  /// `SYMLINK_REF` (info), printed `symlinkRef`.
  SymlinkRef,
  /// `REF_MISSING_NEWLINE` (info), printed `refMissingNewline`.
  RefMissingNewline,
  /// `SYMREF_TARGET_IS_NOT_A_REF` (info), printed `symrefTargetIsNotARef`.
  SymrefTargetIsNotARef,
  /// `TRAILING_REF_CONTENT` (info), printed `trailingRefContent`.
  TrailingRefContent,
  /// `EXTRA_HEADER_ENTRY` (ignore), printed `extraHeaderEntry`.
  ExtraHeaderEntry,
}

impl FsckId {
  /// Format: every id, in fsck.h's order (`FOREACH_FSCK_MSG_ID`, 71 in git 2.50.1).
  pub const ALL: [FsckId; 71] = [
    FsckId::NulInHeader,
    FsckId::UnterminatedHeader,
    FsckId::BadDate,
    FsckId::BadDateOverflow,
    FsckId::BadEmail,
    FsckId::BadName,
    FsckId::BadObjectSha1,
    FsckId::BadPackedRefEntry,
    FsckId::BadPackedRefHeader,
    FsckId::BadParentSha1,
    FsckId::BadRefContent,
    FsckId::BadRefFiletype,
    FsckId::BadRefName,
    FsckId::BadReferentName,
    FsckId::BadTimezone,
    FsckId::BadTree,
    FsckId::BadTreeSha1,
    FsckId::BadType,
    FsckId::DuplicateEntries,
    FsckId::MissingAuthor,
    FsckId::MissingCommitter,
    FsckId::MissingEmail,
    FsckId::MissingNameBeforeEmail,
    FsckId::MissingObject,
    FsckId::MissingSpaceBeforeDate,
    FsckId::MissingSpaceBeforeEmail,
    FsckId::MissingTag,
    FsckId::MissingTagEntry,
    FsckId::MissingTree,
    FsckId::MissingType,
    FsckId::MissingTypeEntry,
    FsckId::MultipleAuthors,
    FsckId::PackedRefEntryNotTerminated,
    FsckId::PackedRefUnsorted,
    FsckId::TreeNotSorted,
    FsckId::UnknownType,
    FsckId::ZeroPaddedDate,
    FsckId::GitmodulesMissing,
    FsckId::GitmodulesBlob,
    FsckId::GitmodulesLarge,
    FsckId::GitmodulesName,
    FsckId::GitmodulesSymlink,
    FsckId::GitmodulesUrl,
    FsckId::GitmodulesPath,
    FsckId::GitmodulesUpdate,
    FsckId::GitattributesMissing,
    FsckId::GitattributesLarge,
    FsckId::GitattributesLineLength,
    FsckId::GitattributesBlob,
    FsckId::EmptyName,
    FsckId::FullPathname,
    FsckId::HasDot,
    FsckId::HasDotdot,
    FsckId::HasDotgit,
    FsckId::NullSha1,
    FsckId::ZeroPaddedFilemode,
    FsckId::NulInCommit,
    FsckId::LargePathname,
    FsckId::BadFilemode,
    FsckId::EmptyPackedRefsFile,
    FsckId::GitmodulesParse,
    FsckId::GitignoreSymlink,
    FsckId::GitattributesSymlink,
    FsckId::MailmapSymlink,
    FsckId::BadTagName,
    FsckId::MissingTaggerEntry,
    FsckId::SymlinkRef,
    FsckId::RefMissingNewline,
    FsckId::SymrefTargetIsNotARef,
    FsckId::TrailingRefContent,
    FsckId::ExtraHeaderEntry,
  ];

  /// The id's default severity (fsck.h), before any configuration changes it.
  pub const fn severity(self) -> Severity {
    match self {
      FsckId::NulInHeader => Severity::Fatal,
      FsckId::UnterminatedHeader => Severity::Fatal,
      FsckId::BadDate => Severity::Error,
      FsckId::BadDateOverflow => Severity::Error,
      FsckId::BadEmail => Severity::Error,
      FsckId::BadName => Severity::Error,
      FsckId::BadObjectSha1 => Severity::Error,
      FsckId::BadPackedRefEntry => Severity::Error,
      FsckId::BadPackedRefHeader => Severity::Error,
      FsckId::BadParentSha1 => Severity::Error,
      FsckId::BadRefContent => Severity::Error,
      FsckId::BadRefFiletype => Severity::Error,
      FsckId::BadRefName => Severity::Error,
      FsckId::BadReferentName => Severity::Error,
      FsckId::BadTimezone => Severity::Error,
      FsckId::BadTree => Severity::Error,
      FsckId::BadTreeSha1 => Severity::Error,
      FsckId::BadType => Severity::Error,
      FsckId::DuplicateEntries => Severity::Error,
      FsckId::MissingAuthor => Severity::Error,
      FsckId::MissingCommitter => Severity::Error,
      FsckId::MissingEmail => Severity::Error,
      FsckId::MissingNameBeforeEmail => Severity::Error,
      FsckId::MissingObject => Severity::Error,
      FsckId::MissingSpaceBeforeDate => Severity::Error,
      FsckId::MissingSpaceBeforeEmail => Severity::Error,
      FsckId::MissingTag => Severity::Error,
      FsckId::MissingTagEntry => Severity::Error,
      FsckId::MissingTree => Severity::Error,
      FsckId::MissingType => Severity::Error,
      FsckId::MissingTypeEntry => Severity::Error,
      FsckId::MultipleAuthors => Severity::Error,
      FsckId::PackedRefEntryNotTerminated => Severity::Error,
      FsckId::PackedRefUnsorted => Severity::Error,
      FsckId::TreeNotSorted => Severity::Error,
      FsckId::UnknownType => Severity::Error,
      FsckId::ZeroPaddedDate => Severity::Error,
      FsckId::GitmodulesMissing => Severity::Error,
      FsckId::GitmodulesBlob => Severity::Error,
      FsckId::GitmodulesLarge => Severity::Error,
      FsckId::GitmodulesName => Severity::Error,
      FsckId::GitmodulesSymlink => Severity::Error,
      FsckId::GitmodulesUrl => Severity::Error,
      FsckId::GitmodulesPath => Severity::Error,
      FsckId::GitmodulesUpdate => Severity::Error,
      FsckId::GitattributesMissing => Severity::Error,
      FsckId::GitattributesLarge => Severity::Error,
      FsckId::GitattributesLineLength => Severity::Error,
      FsckId::GitattributesBlob => Severity::Error,
      FsckId::EmptyName => Severity::Warn,
      FsckId::FullPathname => Severity::Warn,
      FsckId::HasDot => Severity::Warn,
      FsckId::HasDotdot => Severity::Warn,
      FsckId::HasDotgit => Severity::Warn,
      FsckId::NullSha1 => Severity::Warn,
      FsckId::ZeroPaddedFilemode => Severity::Warn,
      FsckId::NulInCommit => Severity::Warn,
      FsckId::LargePathname => Severity::Warn,
      FsckId::BadFilemode => Severity::Info,
      FsckId::EmptyPackedRefsFile => Severity::Info,
      FsckId::GitmodulesParse => Severity::Info,
      FsckId::GitignoreSymlink => Severity::Info,
      FsckId::GitattributesSymlink => Severity::Info,
      FsckId::MailmapSymlink => Severity::Info,
      FsckId::BadTagName => Severity::Info,
      FsckId::MissingTaggerEntry => Severity::Info,
      FsckId::SymlinkRef => Severity::Info,
      FsckId::RefMissingNewline => Severity::Info,
      FsckId::SymrefTargetIsNotARef => Severity::Info,
      FsckId::TrailingRefContent => Severity::Info,
      FsckId::ExtraHeaderEntry => Severity::Ignore,
    }
  }

  /// The id as git prints it, and as `fsck.<id>` configuration names it (`prepare_msg_ids`: the words of its C name in
  /// lower case, each after the first capitalized, joined).
  pub const fn name(self) -> &'static str {
    match self {
      FsckId::NulInHeader => "nulInHeader",
      FsckId::UnterminatedHeader => "unterminatedHeader",
      FsckId::BadDate => "badDate",
      FsckId::BadDateOverflow => "badDateOverflow",
      FsckId::BadEmail => "badEmail",
      FsckId::BadName => "badName",
      FsckId::BadObjectSha1 => "badObjectSha1",
      FsckId::BadPackedRefEntry => "badPackedRefEntry",
      FsckId::BadPackedRefHeader => "badPackedRefHeader",
      FsckId::BadParentSha1 => "badParentSha1",
      FsckId::BadRefContent => "badRefContent",
      FsckId::BadRefFiletype => "badRefFiletype",
      FsckId::BadRefName => "badRefName",
      FsckId::BadReferentName => "badReferentName",
      FsckId::BadTimezone => "badTimezone",
      FsckId::BadTree => "badTree",
      FsckId::BadTreeSha1 => "badTreeSha1",
      FsckId::BadType => "badType",
      FsckId::DuplicateEntries => "duplicateEntries",
      FsckId::MissingAuthor => "missingAuthor",
      FsckId::MissingCommitter => "missingCommitter",
      FsckId::MissingEmail => "missingEmail",
      FsckId::MissingNameBeforeEmail => "missingNameBeforeEmail",
      FsckId::MissingObject => "missingObject",
      FsckId::MissingSpaceBeforeDate => "missingSpaceBeforeDate",
      FsckId::MissingSpaceBeforeEmail => "missingSpaceBeforeEmail",
      FsckId::MissingTag => "missingTag",
      FsckId::MissingTagEntry => "missingTagEntry",
      FsckId::MissingTree => "missingTree",
      FsckId::MissingType => "missingType",
      FsckId::MissingTypeEntry => "missingTypeEntry",
      FsckId::MultipleAuthors => "multipleAuthors",
      FsckId::PackedRefEntryNotTerminated => "packedRefEntryNotTerminated",
      FsckId::PackedRefUnsorted => "packedRefUnsorted",
      FsckId::TreeNotSorted => "treeNotSorted",
      FsckId::UnknownType => "unknownType",
      FsckId::ZeroPaddedDate => "zeroPaddedDate",
      FsckId::GitmodulesMissing => "gitmodulesMissing",
      FsckId::GitmodulesBlob => "gitmodulesBlob",
      FsckId::GitmodulesLarge => "gitmodulesLarge",
      FsckId::GitmodulesName => "gitmodulesName",
      FsckId::GitmodulesSymlink => "gitmodulesSymlink",
      FsckId::GitmodulesUrl => "gitmodulesUrl",
      FsckId::GitmodulesPath => "gitmodulesPath",
      FsckId::GitmodulesUpdate => "gitmodulesUpdate",
      FsckId::GitattributesMissing => "gitattributesMissing",
      FsckId::GitattributesLarge => "gitattributesLarge",
      FsckId::GitattributesLineLength => "gitattributesLineLength",
      FsckId::GitattributesBlob => "gitattributesBlob",
      FsckId::EmptyName => "emptyName",
      FsckId::FullPathname => "fullPathname",
      FsckId::HasDot => "hasDot",
      FsckId::HasDotdot => "hasDotdot",
      FsckId::HasDotgit => "hasDotgit",
      FsckId::NullSha1 => "nullSha1",
      FsckId::ZeroPaddedFilemode => "zeroPaddedFilemode",
      FsckId::NulInCommit => "nulInCommit",
      FsckId::LargePathname => "largePathname",
      FsckId::BadFilemode => "badFilemode",
      FsckId::EmptyPackedRefsFile => "emptyPackedRefsFile",
      FsckId::GitmodulesParse => "gitmodulesParse",
      FsckId::GitignoreSymlink => "gitignoreSymlink",
      FsckId::GitattributesSymlink => "gitattributesSymlink",
      FsckId::MailmapSymlink => "mailmapSymlink",
      FsckId::BadTagName => "badTagName",
      FsckId::MissingTaggerEntry => "missingTaggerEntry",
      FsckId::SymlinkRef => "symlinkRef",
      FsckId::RefMissingNewline => "refMissingNewline",
      FsckId::SymrefTargetIsNotARef => "symrefTargetIsNotARef",
      FsckId::TrailingRefContent => "trailingRefContent",
      FsckId::ExtraHeaderEntry => "extraHeaderEntry",
    }
  }
}

impl FsckId {
  /// The id git prints as `name` ([`FsckId::name`]), compared exactly; none for any other.
  pub fn from_name(name: &str) -> Option<FsckId> {
    FsckId::ALL.into_iter().find(|id| id.name() == name)
  }
}

/// What a check found in one object: each finding in the order git reports it, under one strictness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
  /// The findings, ignored ones included, each once: at most one of each id, so bounded by [`FsckId::ALL`].
  findings: Vec<FsckId>,
  /// The strictness the check ran under.
  strictness: Strictness,
}

impl Report {
  /// An empty report of a check under `strictness`.
  pub(crate) fn new(strictness: Strictness) -> Self {
    Self {
      findings: Vec::new(),
      strictness,
    }
  }

  /// Records `id` at its first report, and says whether it refuses the object, where a commit's or a tag's check
  /// stops. git prints a tree's symbolic-link findings once for each such entry; one types the finding, and keeps the
  /// report bounded by the ids there are.
  pub(crate) fn report(&mut self, id: FsckId) -> bool {
    if !self.findings.contains(&id) {
      self.findings.push(id);
    }
    id.severity().reported(self.strictness) == Severity::Error
  }

  /// The strictness the check ran under.
  pub fn strictness(&self) -> Strictness {
    self.strictness
  }

  /// Every finding in git's order, ignored ones included.
  pub fn findings(&self) -> &[FsckId] {
    &self.findings
  }

  /// What git reports, in its order: each finding but the ignored ones, with the level it is reported at
  /// ([`Severity::Error`] or [`Severity::Warn`]).
  pub fn reported(&self) -> impl Iterator<Item = (FsckId, Severity)> + '_ {
    let strictness = self.strictness;
    self
      .findings
      .iter()
      .map(move |id| (*id, id.severity().reported(strictness)))
      .filter(|(_, level)| *level != Severity::Ignore)
  }

  /// The typed refusal: the first finding reported as an error, if any.
  pub fn refusal(&self) -> Option<FsckId> {
    self
      .reported()
      .find(|(_, level)| *level == Severity::Error)
      .map(|(id, _)| id)
  }

  /// The typed warnings: the findings reported as warnings, which keep the object.
  pub fn warnings(&self) -> impl Iterator<Item = FsckId> + '_ {
    self
      .reported()
      .filter(|(_, level)| *level == Severity::Warn)
      .map(|(id, _)| id)
  }
}
