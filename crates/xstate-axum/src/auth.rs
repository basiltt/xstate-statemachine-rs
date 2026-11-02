//! Authorization and the caller's identity.
//!
//! * [`Authorizer`] replaces Python's `authorize=` callables: `(principal, machine, key, event)
//!   -> Result<(), Forbidden>`; `event` is `None` for reads and stream connects (X0.1).
//! * [`allow_all`] is the explicit opt-out; it logs a warning the first time it is used.
//! * [`PrincipalResolver`] names the authenticated caller from the request (Python
//!   `principal=`); the [`Actor`] extractor exposes it to custom handlers, and
//!   [`principal_or_401`] applies the core rule (`validate_principal`) where a principal is
//!   required (idempotency scope, X0.2).

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use xstate_persistence::StoreError;

use crate::problem::ApiError;
use crate::registry::StatechartRegistry;

/// What an [`Authorizer`] decides about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthRequest<'a> {
    /// The caller, as resolved by the registry's [`PrincipalResolver`] (`None` when there is
    /// no resolver or it identified nobody).
    pub principal: Option<&'a str>,
    /// Registered machine name.
    pub machine: &'a str,
    /// Instance key.
    pub key: &'a str,
    /// The event to send; `None` for reads and stream connects.
    pub event: Option<&'a str>,
}

/// An authorizer's refusal (HTTP 403 / WebSocket close 1008).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, thiserror::Error)]
#[error("forbidden")]
pub struct Forbidden;

/// Decides whether a caller may read or drive an instance.
pub trait Authorizer: Send + Sync {
    /// Admit (`Ok`) or refuse the request.
    ///
    /// # Errors
    /// [`Forbidden`] to refuse.
    fn authorize(&self, request: &AuthRequest<'_>) -> Result<(), Forbidden>;
}

impl<F> Authorizer for F
where
    F: Fn(&AuthRequest<'_>) -> Result<(), Forbidden> + Send + Sync,
{
    fn authorize(&self, request: &AuthRequest<'_>) -> Result<(), Forbidden> {
        self(request)
    }
}

static ALLOW_ALL_WARNED: AtomicBool = AtomicBool::new(false);

/// The authorizer that admits everything. Logs a WARNING the first time it is used so that an
/// accidental production deployment is visible (X0.1).
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(&self, request: &AuthRequest<'_>) -> Result<(), Forbidden> {
        if !ALLOW_ALL_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: "xstate::axum",
                machine = request.machine,
                "`allow_all` authorizer in use: every client may read and drive every instance. \
                 Do not ship this to production (X0.1)."
            );
        }
        Ok(())
    }
}

/// [`AllowAll`] as a shared authorizer, for [`crate::RegisterOptions::new`]. There is no
/// default authorizer: passing this is the explicit opt-out.
#[must_use]
pub fn allow_all() -> Arc<dyn Authorizer> {
    Arc::new(AllowAll)
}

/// Names the authenticated caller of a request (Python `principal=`).
pub trait PrincipalResolver: Send + Sync {
    /// The caller, or `None` when the request identifies nobody.
    fn principal(&self, parts: &Parts) -> Option<String>;
}

impl<F> PrincipalResolver for F
where
    F: Fn(&Parts) -> Option<String> + Send + Sync,
{
    fn principal(&self, parts: &Parts) -> Option<String> {
        self(parts)
    }
}

/// The core principal rule (`validate_principal`) with a refusal mapped to 401: never a pooled
/// `"None"` / `"anonymous"` idempotency scope (battle #303 review H1).
///
/// # Errors
/// [`ApiError::Unauthenticated`] carrying the rule's message as its title.
pub fn principal_or_401(value: Option<&str>) -> Result<String, ApiError> {
    let value = value.unwrap_or("");
    xstate_persistence::validate_principal(value).map_err(|e| match e {
        StoreError::InvalidKey(message) => ApiError::Unauthenticated(message),
        other => ApiError::Unauthenticated(other.to_string()),
    })?;
    Ok(value.to_owned())
}

/// Extractor yielding the caller resolved by the registry's [`PrincipalResolver`] (`None`
/// without a resolver). Works in any router whose state provides the registry through
/// [`FromRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor(pub Option<String>);

impl<S> FromRequestParts<S> for Actor
where
    S: Send + Sync,
    Arc<StatechartRegistry>: FromRef<S>,
{
    type Rejection = Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let registry = Arc::<StatechartRegistry>::from_ref(state);
        std::future::ready(Ok(Self(registry.principal_of(parts))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_principals_are_401() {
        assert!(matches!(principal_or_401(None), Err(ApiError::Unauthenticated(_))));
        assert!(matches!(principal_or_401(Some("None")), Err(ApiError::Unauthenticated(_))));
        assert_eq!(principal_or_401(Some("alice")).ok().as_deref(), Some("alice"));
    }

    #[test]
    fn allow_all_admits() {
        let req = AuthRequest { principal: None, machine: "m", key: "k", event: None };
        assert_eq!(AllowAll.authorize(&req), Ok(()));
    }
}
