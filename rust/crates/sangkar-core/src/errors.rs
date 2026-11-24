//! The closed error-code union and the single error type that crosses a boundary.
//!
//! Ported from `src/shared/errors.ts`. Adding a code is a visible diff: a new failure mode is
//! expressed by one of the twelve, never by a free-form string.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The twelve codes, in the order the TypeScript module publishes them.
pub const APP_ERROR_CODES: [&str; 12] = [
    "wrong_project_db",
    "unsupported_filesystem",
    "schema_too_new",
    "cursor_invalid",
    "cursor_expired",
    "cursor_mismatch",
    "budget_too_large",
    "bootstrapping",
    "not_found",
    "ambiguous",
    "index_locked",
    "internal",
];

/// One of the twelve codes.
///
/// The inner string is private and the associated constants below are the only constructors, so a
/// misspelt code (`ErrorCode::NOT_FOUD`) is a compile error instead of a runtime coercion to
/// `internal`: nothing that is not in the union can reach the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ErrorCode(&'static str);

impl ErrorCode {
    pub const WRONG_PROJECT_DB: Self = Self("wrong_project_db");
    pub const UNSUPPORTED_FILESYSTEM: Self = Self("unsupported_filesystem");
    pub const SCHEMA_TOO_NEW: Self = Self("schema_too_new");
    pub const CURSOR_INVALID: Self = Self("cursor_invalid");
    pub const CURSOR_EXPIRED: Self = Self("cursor_expired");
    pub const CURSOR_MISMATCH: Self = Self("cursor_mismatch");
    pub const BUDGET_TOO_LARGE: Self = Self("budget_too_large");
    pub const BOOTSTRAPPING: Self = Self("bootstrapping");
    pub const NOT_FOUND: Self = Self("not_found");
    pub const AMBIGUOUS: Self = Self("ambiguous");
    pub const INDEX_LOCKED: Self = Self("index_locked");
    pub const INTERNAL: Self = Self("internal");

    /// The same twelve, in the same order as [`APP_ERROR_CODES`].
    pub const ALL: [Self; 12] = [
        Self::WRONG_PROJECT_DB,
        Self::UNSUPPORTED_FILESYSTEM,
        Self::SCHEMA_TOO_NEW,
        Self::CURSOR_INVALID,
        Self::CURSOR_EXPIRED,
        Self::CURSOR_MISMATCH,
        Self::BUDGET_TOO_LARGE,
        Self::BOOTSTRAPPING,
        Self::NOT_FOUND,
        Self::AMBIGUOUS,
        Self::INDEX_LOCKED,
        Self::INTERNAL,
    ];

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl AsRef<str> for ErrorCode {
    fn as_ref(&self) -> &str {
        self.0
    }
}

impl From<ErrorCode> for &'static str {
    fn from(code: ErrorCode) -> Self {
        code.0
    }
}

/// Lets a payload's code be compared to its spelling without unwrapping it: `code == "not_found"`.
impl PartialEq<&str> for ErrorCode {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl Serialize for ErrorCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for ErrorCode {
    /// A payload from another process is data, so an unknown code is refused here rather than
    /// coerced: a code the kernel does not know is a contract break, not a spelling to forgive.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::ALL
            .iter()
            .copied()
            .find(|code| code.0 == raw)
            .ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "unknown error code {raw:?}; expected one of {}",
                    APP_ERROR_CODES.join(", ")
                ))
            })
    }
}

/// The single error type that crosses a boundary.
///
/// The plan's sketch spells `code` as `&'static str`; it is [`ErrorCode`] here — the same
/// `&'static str` behind a closed newtype — because a bare string would let a misspelt code
/// compile and only surface at runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
}

impl AppError {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
            retryable: false,
            retry_after_ms: None,
        }
    }

    /// The fallback for a failure that has no code of its own: a handler bug must not leak a
    /// non-contract code, so the unknown case becomes `internal` with the message it threw.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::INTERNAL, message)
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    #[must_use]
    pub fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    #[must_use]
    pub fn with_retry_after_ms(mut self, retry_after_ms: u64) -> Self {
        self.retry_after_ms = Some(retry_after_ms);
        self
    }

    /// The envelope's `error` object for this error.
    #[must_use]
    pub fn payload(&self) -> ErrorPayload {
        to_error_payload(self)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

/// The envelope's `error` object. Optional members are omitted, never present as `null`: the
/// TypeScript side deletes them, and a key that is present is a different byte string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl ErrorPayload {
    #[must_use]
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
            retryable: None,
            retry_after_ms: None,
        }
    }
}

/// Converts an [`AppError`] into the envelope's error payload.
///
/// `retryable` is written only when it is true, matching the TypeScript: a present `false` would be
/// a key the reference implementation never emits.
#[must_use]
pub fn to_error_payload(e: &AppError) -> ErrorPayload {
    ErrorPayload {
        code: e.code,
        message: e.message.clone(),
        hint: e.hint.clone(),
        retryable: e.retryable.then_some(true),
        retry_after_ms: e.retry_after_ms,
    }
}

/// The payload for a failure that is not an [`AppError`] — the `toErrorPayload(unknown)` branch of
/// the TypeScript, where anything thrown without a code becomes `internal` with its message.
#[must_use]
pub fn to_internal_payload(message: impl Into<String>) -> ErrorPayload {
    to_error_payload(&AppError::internal(message))
}
