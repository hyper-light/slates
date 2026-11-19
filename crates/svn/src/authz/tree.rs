//! The rules one principal meets (A-606): not built yet; the tests come first.

use super::model::Model;
use super::{Access, GlobalRights, Principal, RepositoryPath};

/// The rules one principal meets in one repository: not built yet.
#[derive(Clone, Debug)]
pub struct PrincipalRules;

impl PrincipalRules {
  /// Compiles nothing yet.
  pub(super) fn compile(_model: &Model, _repository: &[u8], _principal: Principal<'_>) -> Self {
    Self
  }

  /// What the principal may do at `path`: not built yet.
  pub fn access(&self, _path: RepositoryPath<'_>) -> Access {
    Access::None
  }

  /// What the principal may do at `path` and beneath it: not built yet.
  pub fn access_beneath(&self, _path: RepositoryPath<'_>) -> Access {
    Access::None
  }

  /// What the principal may do somewhere: not built yet.
  pub fn access_anywhere(&self) -> Access {
    Access::None
  }

  /// The uniform access: not built yet.
  pub fn uniform(&self) -> Option<Access> {
    None
  }

  /// How many answers the uniform access gave: not built yet.
  pub fn uniform_answers(&self) -> u64 {
    0
  }
}

/// What `principal` may do anywhere in `repository`: not built yet.
pub(super) fn global_rights(
  _model: &Model,
  _repository: &[u8],
  _principal: Principal<'_>,
) -> GlobalRights {
  GlobalRights {
    every_path: Access::None,
    some_path: Access::None,
    explicit: false,
  }
}
