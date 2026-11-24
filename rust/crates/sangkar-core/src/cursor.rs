//! Stateless, signed pagination cursors (DATABASE.md §8), ported from `src/shared/cursor.ts`.
//!
//! No cursor table, no server memory: the token *is* the state, so a page boundary survives a
//! restart and a stale token is refused instead of silently addressing a different row.
//!
//! Layout, byte for byte what the TypeScript mints:
//!
//! ```text
//! token      = base64url_nopad(body_ascii) "." base64url_nopad(HMAC_SHA256(key, body_ascii)[0..16])
//! body_ascii = {"generation":…,"tool":…,"argsHash":…,"position":…,"repo":…}   // this key order
//! ```
//!
//! The MAC is over the *encoded* body segment rather than the JSON, which keeps verification a
//! single pass over the bytes that arrived; and the key order is part of the format because the
//! MAC covers JSON text, so `serde_json`'s `preserve_order` feature is load-bearing here.
//!
//! The key is a parameter, never read from the environment — `config` is the only module that
//! touches `SANGKAR_CURSOR_KEY` and hands it to [`Cursor::new`].
//!
//! Two divergences from the TypeScript, both deliberate and both in the safe direction:
//!
//! - `URL_SAFE_NO_PAD` rejects a tag whose final base64 digit carries non-zero trailing bits,
//!   where Node's `Buffer.from(…, "base64url")` silently ignores them. A token Node would accept as
//!   a byte-identical MAC the Rust side refuses as `cursor_invalid`; the reverse never happens.
//! - [`Cursor::sign`] cannot fail, because the payload's shape is in the type. The TypeScript's
//!   runtime check for empty `generation`/`tool`/`argsHash` has no counterpart to throw from here,
//!   and an empty field still cannot verify: [`wire_of`] requires non-empty strings.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use constant_time_eq::constant_time_eq;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value};
use sha2::Sha256;

use crate::errors::{AppError, ErrorCode};

/// Minimum key material. An HMAC-SHA256 key shorter than the hash's 256-bit output buys nothing
/// and is the usual way a "secret" becomes guessable, so a short key is a startup failure.
pub const CURSOR_KEY_MIN_BYTES: usize = 32;

/// Truncated tag width: 128 bits, the standard MAC truncation floor, and a shorter token.
pub const CURSOR_TAG_BYTES: usize = 16;

/// Refuse absurd client input before decoding it. Real tokens are ~150 characters, and a cursor is
/// untrusted input, so its size is bounded like any other.
pub const MAX_TOKEN_CHARS: usize = 1024;

/// DESIGN.md §7.2 fixes this wording: an expired cursor tells the agent to re-run, not to guess.
const HINT_RE_RUN: &str = "re-run the original call";

/// The refusal hint for anything that did not arrive as a genuine token.
const HINT_INVALID: &str = "re-run the original call to mint a fresh cursor";

/// The refusal hint for a genuine token minted for a different call.
const HINT_MISMATCH: &str = "re-run the original call with the same tool and arguments";

/// `Number.isSafeInteger`'s upper bound. A position is an index into a generation's ordering, so a
/// number a JavaScript reader would round is not a position at all.
const MAX_SAFE_POSITION: u64 = 9_007_199_254_740_991;

/// What a cursor pins: the generation it was minted in, the call it belongs to, and the offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorPayload {
    pub generation: String,
    pub tool: String,
    /// The per-tool argument digest. Its *shape* differs per tool (`sha256:<hex>` for
    /// trace/expand/status/summary, bare hex for search, 32-char truncation for impact) and the
    /// cursor does not care: it pins whatever the handler computed.
    pub args_hash: String,
    /// Items already delivered; an index into the generation's stable ordering.
    pub position: u64,
}

/// What the resuming call claims. Anything outside this must not be served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorExpectation {
    pub generation: String,
    pub tool: String,
    pub args_hash: String,
}

/// A cursor that did not verify. `hint` is a `&'static str`, so it is always present and always
/// one of the three fixed sentences — a caller cannot drop it or paraphrase it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorRefusal {
    pub code: ErrorCode,
    pub hint: &'static str,
}

/// The result of [`Cursor::verify`]: the position it addressed, or why it was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorResult {
    Accepted { position: u64 },
    Refused(CursorRefusal),
}

impl CursorResult {
    /// The addressed position, or `None` for a refusal.
    #[must_use]
    pub fn position(&self) -> Option<u64> {
        match *self {
            Self::Accepted { position } => Some(position),
            Self::Refused(_) => None,
        }
    }

    /// The refusal, or `None` when the cursor was accepted.
    #[must_use]
    pub fn refusal(&self) -> Option<CursorRefusal> {
        match *self {
            Self::Accepted { .. } => None,
            Self::Refused(refusal) => Some(refusal),
        }
    }
}

/// The cursor seam a tool receives: bound to one key and one repository, so no handler ever holds
/// the HMAC secret or has to name a repo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    key: Vec<u8>,
    /// `None` is the unbound, single-repository facade that predates the daemon; it stamps `""`,
    /// which is what the TypeScript's `repoId ?? ""` does.
    repo: Option<String>,
}

impl Cursor {
    /// Binds the key and the repository once, at the composition root. The key-length check happens
    /// here so a misconfigured server fails at startup rather than at the first truncation.
    ///
    /// # Errors
    ///
    /// `AppError` with code `internal` when the key is shorter than [`CURSOR_KEY_MIN_BYTES`]: that
    /// is a configuration bug, never client input.
    pub fn new(key: impl Into<Vec<u8>>, repo: Option<String>) -> Result<Self, AppError> {
        let key = key.into();
        if key.len() < CURSOR_KEY_MIN_BYTES {
            return Err(AppError::new(
                ErrorCode::INTERNAL,
                format!("cursor key must be at least {CURSOR_KEY_MIN_BYTES} bytes of key material"),
            )
            .with_hint("set SANGKAR_CURSOR_KEY to a random value of 32 bytes or more"));
        }
        Ok(Self { key, repo })
    }

    /// Mints a cursor. Signing is deterministic for a given `(payload, key)` — there is no
    /// issued-at field, because expiry is generation-bound (DATABASE.md §8) and a timestamp
    /// nothing reads would only make the token un-reproducible in tests.
    #[must_use]
    pub fn sign(&self, payload: &CursorPayload) -> String {
        let body = URL_SAFE_NO_PAD.encode(encode_body(payload, self.repo.as_deref().unwrap_or("")));
        let tag = tag_for(&self.key, &body);
        format!("{body}.{}", URL_SAFE_NO_PAD.encode(tag))
    }

    /// Verifies a cursor in the order DATABASE.md §8 fixes: shape, then MAC, then generation, then
    /// call identity. The order is load-bearing — a token from an older generation must read as
    /// `cursor_expired` (re-run) rather than `cursor_mismatch`, and a tampered token as
    /// `cursor_invalid` rather than either.
    #[must_use]
    pub fn verify(&self, token: &str, expected: &CursorExpectation) -> CursorResult {
        // UTF-16 length, which is what the TypeScript's `token.length` counts: for the limit to
        // differ, a token would have to carry astral characters, and it would then be refused by
        // the base64 decoder instead. Same code, same hint, same order.
        if token.is_empty() || token.encode_utf16().count() > MAX_TOKEN_CHARS {
            return refuse(ErrorCode::CURSOR_INVALID, HINT_INVALID);
        }
        let Some(dot) = token.find('.') else {
            return refuse(ErrorCode::CURSOR_INVALID, HINT_INVALID);
        };
        if dot == 0 || dot == token.len() - 1 || token[dot + 1..].contains('.') {
            return refuse(ErrorCode::CURSOR_INVALID, HINT_INVALID);
        }
        let (body, tag_text) = (&token[..dot], &token[dot + 1..]);

        // Length before the compare, never a compare of different-length slices: `constant_time_eq`
        // is only defined on equal lengths, and `timingSafeEqual` throws on a mismatch.
        let Ok(tag) = URL_SAFE_NO_PAD.decode(tag_text) else {
            return refuse(ErrorCode::CURSOR_INVALID, HINT_INVALID);
        };
        if tag.len() != CURSOR_TAG_BYTES || !constant_time_eq(&tag, &tag_for(&self.key, body)) {
            return refuse(ErrorCode::CURSOR_INVALID, HINT_INVALID);
        }

        // Only a key holder can reach this; a signed-but-unparseable payload is still not a page.
        let decoded = URL_SAFE_NO_PAD
            .decode(body)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let Some(decoded) = decoded else {
            return refuse(ErrorCode::CURSOR_INVALID, HINT_INVALID);
        };
        let Some(wire) = wire_of(&decoded) else {
            return refuse(ErrorCode::CURSOR_INVALID, HINT_INVALID);
        };

        if wire.generation != expected.generation {
            return refuse(ErrorCode::CURSOR_EXPIRED, HINT_RE_RUN);
        }
        // Repo is part of the call's identity — the same tool and arguments in another repository
        // are still another page — so it joins the mismatch check, after the MAC proved the stamp
        // genuine.
        if wire.repo != self.repo.as_deref().unwrap_or("") {
            return refuse(ErrorCode::CURSOR_MISMATCH, HINT_MISMATCH);
        }
        if wire.tool != expected.tool || wire.args_hash != expected.args_hash {
            return refuse(ErrorCode::CURSOR_MISMATCH, HINT_MISMATCH);
        }
        CursorResult::Accepted {
            position: wire.position,
        }
    }
}

fn refuse(code: ErrorCode, hint: &'static str) -> CursorResult {
    CursorResult::Refused(CursorRefusal { code, hint })
}

/// The five members a token must carry, narrowed to their checked types. Borrowed from the parsed
/// JSON so verification allocates nothing beyond the parse itself.
struct Wire<'a> {
    generation: &'a str,
    tool: &'a str,
    args_hash: &'a str,
    position: u64,
    repo: &'a str,
}

/// The compact JSON of the payload plus the facade's `repo` stamp.
///
/// This function is the *only* place the key order is written down. `serde_json`'s `preserve_order`
/// feature keeps insertion order in a `Map`, so inserting `generation`, `tool`, `argsHash`,
/// `position`, `repo` in that sequence — and nowhere else — is what makes a token byte-identical to
/// the TypeScript's `JSON.stringify`.
fn encode_body(payload: &CursorPayload, repo: &str) -> String {
    let mut wire = Map::new();
    wire.insert(
        "generation".to_owned(),
        Value::String(payload.generation.clone()),
    );
    wire.insert("tool".to_owned(), Value::String(payload.tool.clone()));
    wire.insert(
        "argsHash".to_owned(),
        Value::String(payload.args_hash.clone()),
    );
    wire.insert("position".to_owned(), Value::from(payload.position));
    wire.insert("repo".to_owned(), Value::String(repo.to_owned()));
    // Serialising a `Map` of strings and an integer cannot fail: no floats to reject, no
    // non-string keys.
    Value::Object(wire).to_string()
}

/// The first [`CURSOR_TAG_BYTES`] of `HMAC-SHA256(key, body)`.
fn tag_for(key: &[u8], body: &str) -> [u8; CURSOR_TAG_BYTES] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts a key of any length");
    mac.update(body.as_bytes());
    let digest = mac.finalize().into_bytes();
    let mut tag = [0_u8; CURSOR_TAG_BYTES];
    tag.copy_from_slice(&digest[..CURSOR_TAG_BYTES]);
    tag
}

/// A decoded token must be an object carrying exactly the shape `isWire` in the TypeScript accepts:
/// four non-empty strings, a safe non-negative integer, and a `repo` stamp that may be empty. Extra
/// members are ignored, not refused, so a token from a newer writer stays readable.
fn wire_of(decoded: &Value) -> Option<Wire<'_>> {
    let fields = decoded.as_object()?;
    Some(Wire {
        generation: non_empty(fields, "generation")?,
        tool: non_empty(fields, "tool")?,
        args_hash: non_empty(fields, "argsHash")?,
        // `Number.isSafeInteger` is `as_u64` (which already rejects floats and negatives) plus the
        // 2^53 - 1 ceiling.
        position: fields
            .get("position")?
            .as_u64()
            .filter(|position| *position <= MAX_SAFE_POSITION)?,
        repo: text_of(fields, "repo")?,
    })
}

fn text_of<'a>(fields: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    fields.get(name)?.as_str()
}

fn non_empty<'a>(fields: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    text_of(fields, name).filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn payload(position: u64) -> CursorPayload {
        CursorPayload {
            generation: "g-7f3a91".to_owned(),
            tool: "trace".to_owned(),
            args_hash: format!("sha256:{}", "a".repeat(64)),
            position,
        }
    }

    #[test]
    fn the_body_has_the_typescripts_key_order() {
        let body = encode_body(&payload(0), "");
        let args_hash = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            body,
            format!(
                r#"{{"generation":"g-7f3a91","tool":"trace","argsHash":"{args_hash}","position":0,"repo":""}}"#
            )
        );
    }

    #[test]
    fn the_tag_is_the_url_safe_alphabet_without_padding() {
        let cursor = Cursor::new(KEY, None).expect("32 bytes is enough");
        let token = cursor.sign(&payload(0));
        let (body, tag) = token.split_once('.').expect("one dot");
        assert!(!token.ends_with('='), "no padding: {tag}");
        assert!(
            body.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
        assert_eq!(tag.len(), 22);
    }

    #[test]
    fn a_short_key_is_an_internal_error() {
        let error = Cursor::new(&KEY[..CURSOR_KEY_MIN_BYTES - 1], None).expect_err("31 bytes");
        assert_eq!(error.code, ErrorCode::INTERNAL);
        assert_eq!(error.code.as_str(), "internal");
        assert_eq!(
            error.hint.as_deref(),
            Some("set SANGKAR_CURSOR_KEY to a random value of 32 bytes or more")
        );
        assert!(Cursor::new(KEY, None).is_ok());
    }
}
