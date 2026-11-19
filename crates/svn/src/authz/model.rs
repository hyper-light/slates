//! The authz file's sections (A-606): not built yet; the tests come first.

use super::{AuthzRefused, Bounds, IgnoredEntry};

/// A parsed configuration: not built yet.
#[derive(Clone, Debug)]
pub(super) struct Model {
  ignored: Vec<IgnoredEntry>,
}

impl Model {
  /// Reads nothing yet.
  pub(super) fn parse(
    _rules: &[u8],
    _groups: Option<&[u8]>,
    _bounds: Bounds,
  ) -> Result<Self, AuthzRefused> {
    Ok(Self {
      ignored: Vec::new(),
    })
  }

  /// The entries ignored.
  pub(super) fn ignored_entries(&self) -> &[IgnoredEntry] {
    &self.ignored
  }
}
