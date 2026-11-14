//! Permissions: what an account may do to each model.

use std::collections::HashMap;

/// Something an account can be allowed to do to a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// See the list and individual records.
    View,
    /// Create records.
    Add,
    /// Edit existing records.
    Change,
    /// Delete records.
    Delete,
}

impl Action {
    /// Every action, in the order they are shown.
    pub const ALL: [Self; 4] = [Self::View, Self::Add, Self::Change, Self::Delete];

    /// The name stored in the database and used in forms: `view`, `add`, `change`, `delete`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::View => "view",
            Self::Add => "add",
            Self::Change => "change",
            Self::Delete => "delete",
        }
    }

    /// The reverse of [`Action::as_str`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.as_str() == name)
    }

    fn bit(self) -> u8 {
        match self {
            Self::View => 1,
            Self::Add => 2,
            Self::Change => 4,
            Self::Delete => 8,
        }
    }
}

/// What one account may do. Returned by [`crate::UserStore::access`] for every request.
///
/// A superuser may do everything. Anyone else may do exactly what was granted, with one
/// addition: whoever may change a model may also view it.
///
/// ```
/// use herapi::{Access, Action};
///
/// let editor = Access::none().grant("post", Action::Change).grant("post", Action::Add);
/// assert!(editor.allows("post", Action::View));
/// assert!(!editor.allows("post", Action::Delete));
/// assert!(!editor.allows("author", Action::View));
/// assert!(Access::superuser().allows("author", Action::Delete));
/// ```
#[derive(Debug, Clone, Default)]
pub struct Access {
    superuser: bool,
    /// Model slug to a bit set of granted actions.
    grants: HashMap<String, u8>,
}

impl Access {
    /// May do everything, including managing accounts.
    #[must_use]
    pub fn superuser() -> Self {
        Self {
            superuser: true,
            grants: HashMap::new(),
        }
    }

    /// May log in but do nothing, until permissions are added with [`Access::grant`].
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Adds one permission. `model` is the model's URL name (its table name by default); file
    /// uploads use `files`.
    #[must_use]
    pub fn grant(mut self, model: impl Into<String>, action: Action) -> Self {
        *self.grants.entry(model.into()).or_default() |= action.bit();
        self
    }

    /// Whether this is a superuser.
    #[must_use]
    pub fn is_superuser(&self) -> bool {
        self.superuser
    }

    /// Whether `action` on `model` is allowed.
    #[must_use]
    pub fn allows(&self, model: &str, action: Action) -> bool {
        if self.superuser {
            return true;
        }
        let granted = self.grants.get(model).copied().unwrap_or(0);
        let implied = if action == Action::View {
            Action::Change.bit()
        } else {
            0
        };
        granted & (action.bit() | implied) != 0
    }

    /// Whether `action` was granted directly, ignoring superuser status and implied
    /// permissions. This is what the account forms show.
    pub(crate) fn granted(&self, model: &str, action: Action) -> bool {
        self.grants.get(model).copied().unwrap_or(0) & action.bit() != 0
    }
}
