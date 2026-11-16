//! Sign-in and role checks.
//!
//! The app puts a [`Verifier`] in its state and takes [`Authenticated`] as a
//! handler argument: requests without a valid `Authorization: Bearer <token>`
//! header are rejected with 401 before the handler runs.
//!
//! * [`dev`] – test sign-in for local development. It exists only in debug
//!   builds, so a release build can never accept dev tokens.
//! * Production – implement [`TokenVerifier`] for your identity provider
//!   (for example an OIDC / JWT verifier) and return it from `verifier()` in main.rs.

use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

/// The signed-in user, as established by a [`TokenVerifier`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// Login name; recorded as the actor on audit entries.
    pub user: String,
    /// Work email; links a login to an employee record (e.g. a manager's reports).
    #[serde(default)]
    pub email: Option<String>,
    /// Role codes, e.g. `ar_admin`. e.g. groups from your identity provider.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl Principal {
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }

    pub fn has_any(&self, roles: &[&str]) -> bool {
        roles.iter().any(|r| self.has_role(r))
    }

    /// `Ok` when the user holds at least one of `roles`, otherwise 403.
    pub fn require_any(&self, roles: &[&str]) -> Result<(), ApiError> {
        if self.has_any(roles) {
            Ok(())
        } else {
            Err(ApiError::forbidden("your role does not allow this"))
        }
    }
}

/// Checks a bearer token and says who it belongs to.
pub trait TokenVerifier: Send + Sync {
    fn verify(&self, token: &str) -> Result<Principal, ApiError>;
    /// Short description for the health check, e.g. "dev sign-in".
    fn describe(&self) -> &'static str;
}

pub type Verifier = Arc<dyn TokenVerifier>;

/// An error response: `{"error": "<message>"}` with an HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, message: message.into() }
    }
    pub fn unauthorized(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, m)
    }
    pub fn forbidden(m: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, m)
    }
    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, m)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.status, self.message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "error": self.message }))).into_response()
    }
}

/// Handler argument that requires a signed-in user.
pub struct Authenticated(pub Principal);

impl<S> FromRequestParts<S> for Authenticated
where
    Verifier: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ApiError::unauthorized("sign in required"))?;
        Verifier::from_ref(state).verify(token).map(Authenticated)
    }
}

/// Development sign-in: a token is just the principal, encoded. Anyone can make
/// one, which is why this module is compiled out of release builds.
#[cfg(debug_assertions)]
pub mod dev {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    use super::{ApiError, Principal, TokenVerifier};

    const PREFIX: &str = "dev.";

    /// A dev token for `principal`.
    pub fn issue(principal: &Principal) -> String {
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(principal).expect("serializable")))
    }

    pub struct DevVerifier;

    impl TokenVerifier for DevVerifier {
        fn verify(&self, token: &str) -> Result<Principal, ApiError> {
            token
                .strip_prefix(PREFIX)
                .and_then(|t| URL_SAFE_NO_PAD.decode(t).ok())
                .and_then(|b| serde_json::from_slice::<Principal>(&b).ok())
                .filter(|p| !p.user.trim().is_empty())
                .ok_or_else(|| ApiError::unauthorized("invalid or expired sign-in"))
        }

        fn describe(&self) -> &'static str {
            "dev sign-in (debug build only)"
        }
    }
}
