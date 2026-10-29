//! The permission a user route requires, declared beside the route in
//! `router.rs` and checked by the user extractors. A user route that declares
//! no gate answers `500`, so a route cannot be added open.

use axum::Extension;
use axum::handler::{Handler, Layered};
use axum::http::request::Parts;

use pipette_plan_types::UserPermission;

use crate::auth::UserRights;
use crate::error::AppError;
use crate::extract::record_path_ids;
use crate::state::AppState;

/// What a user route requires of the caller's permission set.
#[derive(Debug, Clone, Copy)]
pub enum Gate {
    /// Any active account: the route's own scope is the whole check.
    Active,
    Needs(UserPermission),
    AllOf(&'static [UserPermission]),
    /// One of the names, refused with the text given, which says what each
    /// one opens.
    AnyOf(&'static [UserPermission], &'static str),
}

pub fn gate<H, T>(gate: Gate, handler: H) -> Layered<Extension<Gate>, H, T, AppState>
where
    H: Handler<T, AppState>,
    T: 'static,
{
    handler.layer(Extension(gate))
}

impl Gate {
    pub(crate) fn of(parts: &Parts) -> Result<Self, AppError> {
        parts.extensions.get::<Self>().copied().ok_or_else(|| {
            tracing::error!("refused: a user route declares no gate");
            AppError::Internal("this route declares no permission gate".into())
        })
    }

    /// `Ok` when `rights` meets this gate; a `403` naming what it lacks. A
    /// refusal records the route's ids on the span first, as `ApiPath` would
    /// have.
    pub(crate) async fn admit(
        self,
        parts: &mut Parts,
        state: &AppState,
        rights: &UserRights,
    ) -> Result<(), AppError> {
        if self.allows(rights) {
            return Ok(());
        }
        record_path_ids(parts, state).await;
        match self {
            Self::Active => Ok(()),
            Self::Needs(permission) => rights.require(permission),
            Self::AllOf(permissions) => permissions
                .iter()
                .try_for_each(|permission| rights.require(*permission)),
            Self::AnyOf(permissions, refusal) => {
                let names: Vec<&str> = permissions.iter().map(AsRef::as_ref).collect();
                tracing::warn!(required_any = ?names, "refused: user lacks permission");
                Err(AppError::Forbidden(refusal.into()))
            }
        }
    }

    pub(crate) fn allows(self, rights: &UserRights) -> bool {
        match self {
            Self::Active => true,
            Self::Needs(permission) => rights.holds(permission),
            Self::AllOf(permissions) => permissions.iter().all(|p| rights.holds(*p)),
            Self::AnyOf(permissions, _) => permissions.iter().any(|p| rights.holds(*p)),
        }
    }
}
