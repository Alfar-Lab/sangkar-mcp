//! Cross-implementation cursor vectors: what `src/shared/cursor.ts` mints must be what
//! `sangkar_core::cursor` mints, byte for byte, because both serve the same graph during the
//! migration (`RUST-MIGRATION.md` §7 phase 0, §8 R4).
//!
//! Two oracles, and they are different questions:
//!
//! - `rust/tests/vectors/cursor_vectors.json` holds six tokens the **TypeScript** produced
//!   (`scripts/parity/cursor-vectors.ts`, run under Bun 1.4.4). Exact equality against it is the
//!   byte-for-byte claim, and it is what catches a wrong base64 alphabet, a JSON key in the wrong
//!   order, or a MAC taken over the decoded body instead of the encoded one.
//! - the refusals are checked for **order**, not just for codes: a tampered tag must read as
//!   `cursor_invalid` even when the caller also expects an older generation, and an expired token
//!   must read as `cursor_expired` rather than as invalid. Swapping two refusals is a change
//!   nothing else in the suite would notice.

use std::collections::BTreeSet;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sangkar_core::cursor::{
    CURSOR_KEY_MIN_BYTES, CURSOR_TAG_BYTES, Cursor, CursorExpectation, CursorPayload,
    CursorRefusal, CursorResult, MAX_TOKEN_CHARS,
};
use sangkar_core::errors::{AppError, ErrorCode};
use serde_json::Value;

/// The key `scripts/parity/cursor-vectors.ts` signs with. 32 ASCII bytes, the exact floor. Not a
/// secret: a vector only pins a format if both sides hold the same bytes, and the exact-token
/// assertions fail loudly if this ever drifts from the script's `KEY`.
const KEY: &str = "0123456789abcdef0123456789abcdef";

/// The three hints, verbatim from `src/shared/cursor.ts`. DESIGN.md §7.2 fixes the wording: an
/// expired cursor tells the agent to re-run, not to guess.
const HINT_RE_RUN: &str = "re-run the original call";
const HINT_INVALID: &str = "re-run the original call to mint a fresh cursor";
const HINT_MISMATCH: &str = "re-run the original call with the same tool and arguments";

/// One vector as the script printed it: the payload, the repo stamp, and the TypeScript's token.
#[derive(Debug, serde::Deserialize)]
struct Vector {
    /// What the vector covers, quoted in a failing assertion so the message names the payload.
    label: String,
    generation: String,
    tool: String,
    #[serde(rename = "argsHash")]
    args_hash: String,
    position: u64,
    /// `""` is the unbound facade; anything else is the repository a daemon-bound facade stamps.
    repo: String,
    token: String,
}

impl Vector {
    fn payload(&self) -> CursorPayload {
        CursorPayload {
            generation: self.generation.clone(),
            tool: self.tool.clone(),
            args_hash: self.args_hash.clone(),
            position: self.position,
        }
    }

    fn expectation(&self) -> CursorExpectation {
        CursorExpectation {
            generation: self.generation.clone(),
            tool: self.tool.clone(),
            args_hash: self.args_hash.clone(),
        }
    }

    /// The facade this vector was minted by: unbound for `""`, bound to the repo otherwise.
    fn binding(&self) -> Option<String> {
        if self.repo.is_empty() {
            None
        } else {
            Some(self.repo.clone())
        }
    }

    fn cursor(&self) -> Cursor {
        Cursor::new(KEY, self.binding()).expect("the vector key is 32 bytes")
    }

    /// The two halves the format is made of: the encoded body and the encoded tag.
    fn segments(&self) -> (&str, &str) {
        self.token
            .split_once('.')
            .expect("a token is one dot and no more")
    }
}

fn vectors() -> Vec<Vector> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/vectors/cursor_vectors.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is unreadable: {error}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} is not a vector array: {error}", path.display()))
}

fn refusal_of(result: CursorResult) -> CursorRefusal {
    match result {
        CursorResult::Refused(refusal) => refusal,
        CursorResult::Accepted { position } => {
            panic!("expected a refusal, got acceptance at position {position}")
        }
    }
}

/// Replaces the character at `index` with a different base64url digit, so the token keeps its shape
/// and only its content changes — the same trick `tests/cursor.test.ts` uses.
fn flip(text: &str, index: usize) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    let current = chars[index];
    chars[index] = if current == 'A' { 'B' } else { 'A' };
    chars.into_iter().collect()
}

fn decode_segment(segment: &str) -> String {
    let bytes = URL_SAFE_NO_PAD
        .decode(segment)
        .expect("a vector's segment is base64url without padding");
    String::from_utf8(bytes).expect("a vector's segment decodes to UTF-8")
}

#[test]
fn the_vector_file_is_the_six_payloads_the_script_prints() {
    let vectors = vectors();
    assert_eq!(vectors.len(), 6, "six vectors, one per covered payload");

    // Coverage is the point of the set, so it is asserted rather than assumed: dropping a shape or
    // a repo state from the script would otherwise leave a green suite with less evidence in it.
    let shapes: BTreeSet<&str> = vectors.iter().map(|v| shape_of(&v.args_hash)).collect();
    assert_eq!(
        shapes,
        BTreeSet::from(["sha256:<64 hex>", "bare 64 hex", "32-char truncation"]),
        "the three argsHash shapes the handlers really produce"
    );
    let tools: BTreeSet<&str> = vectors.iter().map(|v| v.tool.as_str()).collect();
    assert_eq!(tools.len(), 4, "four tools: {tools:?}");
    let repos: BTreeSet<&str> = vectors
        .iter()
        .map(|v| {
            if v.repo.is_empty() {
                "unbound"
            } else {
                "bound"
            }
        })
        .collect();
    assert_eq!(repos, BTreeSet::from(["unbound", "bound"]));
    let positions: BTreeSet<u64> = vectors.iter().map(|v| v.position).collect();
    assert_eq!(positions, BTreeSet::from([0, 37]));
}

/// Which of the three `argsHash` shapes a value has. The inconsistency is real and is being
/// reproduced, not fixed: `trace`/`expand`/`status`/`summary` prefix `sha256:`, `search` is bare
/// hex, `impact` truncates to 32 characters.
fn shape_of(args_hash: &str) -> &'static str {
    match (args_hash.strip_prefix("sha256:"), args_hash.len()) {
        (Some(hex), _) if hex.len() == 64 && is_hex(hex) => "sha256:<64 hex>",
        (None, 64) if is_hex(args_hash) => "bare 64 hex",
        (None, 32) if is_hex(args_hash) => "32-char truncation",
        _ => "unrecognised",
    }
}

fn is_hex(text: &str) -> bool {
    text.chars().all(|c| c.is_ascii_hexdigit())
}

#[test]
fn six_vectors_mint_byte_identical_tokens() {
    for vector in &vectors() {
        assert_eq!(
            vector.cursor().sign(&vector.payload()),
            vector.token,
            "{}",
            vector.label
        );
    }
}

#[test]
fn every_vector_verifies_to_its_position() {
    for vector in &vectors() {
        let result = vector.cursor().verify(&vector.token, &vector.expectation());
        assert_eq!(result.position(), Some(vector.position), "{}", vector.label);
        // Deterministic: no clock, no salt, no server memory.
        assert_eq!(vector.cursor().sign(&vector.payload()), vector.token);
    }
}

#[test]
fn the_body_is_compact_json_in_the_typescripts_key_order() {
    for vector in &vectors() {
        let (body, _) = vector.segments();
        let text = decode_segment(body);
        assert!(
            !text.chars().any(char::is_whitespace),
            "compact JSON, no spaces: {text}"
        );
        let parsed: Value = serde_json::from_str(&text).expect("the body is JSON");
        let keys: Vec<&str> = parsed
            .as_object()
            .expect("the body is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            ["generation", "tool", "argsHash", "position", "repo"],
            "{}: the key order is part of the MAC",
            vector.label
        );
        assert_eq!(parsed["position"], Value::from(vector.position));
        assert_eq!(parsed["repo"], Value::from(vector.repo.clone()));
    }
}

#[test]
fn tokens_are_base64url_without_padding() {
    // The alphabet, pinned independently of luck: 0xfb 0xff encodes to `-_` in the URL-safe
    // alphabet and `+/` in the standard one, and a wrong alphabet makes every token differ.
    assert_eq!(URL_SAFE_NO_PAD.encode([0xfb_u8, 0xff]), "-_8");

    for vector in &vectors() {
        assert!(!vector.token.contains('='), "no padding: {}", vector.token);
        assert!(!vector.token.contains('+') && !vector.token.contains('/'));
        let (body, tag) = vector.segments();
        assert_eq!(
            tag.len(),
            22,
            "a {CURSOR_TAG_BYTES}-byte tag is 22 base64url digits"
        );
        for segment in [body, tag] {
            assert!(
                segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{}",
                vector.label
            );
        }
    }
}

#[test]
fn a_flipped_body_byte_is_cursor_invalid() {
    for vector in &vectors() {
        let cursor = vector.cursor();
        let (body, tag) = vector.segments();
        let forged = format!("{}.{tag}", flip(body, body.len() / 2));
        let refusal = refusal_of(cursor.verify(&forged, &vector.expectation()));
        assert_eq!(refusal.code, ErrorCode::CURSOR_INVALID, "{}", vector.label);
        assert_eq!(refusal.hint, HINT_INVALID);
        // The order again: same forged token, an expected generation it does not carry, and the
        // MAC still has to be the refusal that fires.
        let expired_view = CursorExpectation {
            generation: "g-000000".to_owned(),
            ..vector.expectation()
        };
        assert_eq!(
            refusal_of(cursor.verify(&forged, &expired_view)).code,
            ErrorCode::CURSOR_INVALID,
            "{}: a bad tag must never report cursor_expired",
            vector.label
        );
    }
}

#[test]
fn a_flipped_tag_byte_is_cursor_invalid() {
    for vector in &vectors() {
        let cursor = vector.cursor();
        let (body, tag) = vector.segments();
        for at in [0, tag.len() / 2, tag.len() - 1] {
            let forged = format!("{body}.{}", flip(tag, at));
            let refusal = refusal_of(cursor.verify(&forged, &vector.expectation()));
            assert_eq!(
                refusal.code,
                ErrorCode::CURSOR_INVALID,
                "{}: tag digit {at}",
                vector.label
            );
            assert_eq!(refusal.hint, HINT_INVALID);
        }
        // A tag of the wrong length is refused before the compare, not compared as a prefix.
        let short = &tag[..tag.len() - 1];
        assert_eq!(
            refusal_of(cursor.verify(&format!("{body}.{short}"), &vector.expectation())).code,
            ErrorCode::CURSOR_INVALID
        );
    }
}

#[test]
fn another_generation_is_cursor_expired() {
    for vector in &vectors() {
        let cursor = vector.cursor();
        let expectation = CursorExpectation {
            generation: "g-000000".to_owned(),
            ..vector.expectation()
        };
        let refusal = refusal_of(cursor.verify(&vector.token, &expectation));
        assert_eq!(refusal.code, ErrorCode::CURSOR_EXPIRED, "{}", vector.label);
        assert_eq!(refusal.hint, HINT_RE_RUN, "the expired hint, verbatim");
        // An expired-but-validly-signed token must not read as invalid or as a mismatch.
        assert_ne!(refusal.code, ErrorCode::CURSOR_INVALID);
        assert_ne!(refusal.code, ErrorCode::CURSOR_MISMATCH);
    }
}

#[test]
fn another_repo_is_cursor_mismatch() {
    let other = "/srv/repos/sangkar-fixture-other";
    for vector in &vectors() {
        let elsewhere = if vector.repo.is_empty() {
            Some(other.to_owned())
        } else {
            None
        };
        let cursor = Cursor::new(KEY, elsewhere).expect("the vector key is 32 bytes");
        let refusal = refusal_of(cursor.verify(&vector.token, &vector.expectation()));
        assert_eq!(refusal.code, ErrorCode::CURSOR_MISMATCH, "{}", vector.label);
        assert_eq!(refusal.hint, HINT_MISMATCH, "the mismatch hint, verbatim");
        // The repo check comes after the generation check and before the tool check.
        assert_ne!(refusal.code, ErrorCode::CURSOR_EXPIRED);
        assert_ne!(refusal.code, ErrorCode::CURSOR_INVALID);
    }
}

#[test]
fn another_tool_or_args_hash_is_cursor_mismatch() {
    for vector in &vectors() {
        let cursor = vector.cursor();
        let other_tool = CursorExpectation {
            tool: "some-other-tool".to_owned(),
            ..vector.expectation()
        };
        let refusal = refusal_of(cursor.verify(&vector.token, &other_tool));
        assert_eq!(
            refusal.code,
            ErrorCode::CURSOR_MISMATCH,
            "{}: tool",
            vector.label
        );
        assert_eq!(refusal.hint, HINT_MISMATCH);
        let other_args = CursorExpectation {
            args_hash: format!("{}-other", vector.args_hash),
            ..vector.expectation()
        };
        let refusal = refusal_of(cursor.verify(&vector.token, &other_args));
        assert_eq!(refusal.code, ErrorCode::CURSOR_MISMATCH, "{}", vector.label);
        assert_eq!(refusal.hint, HINT_MISMATCH);
    }
}

#[test]
fn malformed_shapes_are_cursor_invalid() {
    let vector = &vectors()[0];
    let cursor = vector.cursor();
    let (body, tag) = vector.segments();
    let cases = [
        String::new(),
        body.to_owned(),
        format!(".{tag}"),
        format!("{body}."),
        format!("{body}.{tag}.{tag}"),
        format!("{body}|{tag}"),
    ];
    for case in cases {
        let refusal = refusal_of(cursor.verify(&case, &vector.expectation()));
        assert_eq!(refusal.code, ErrorCode::CURSOR_INVALID, "case {case:?}");
        assert_eq!(refusal.hint, HINT_INVALID);
    }
}

#[test]
fn an_oversized_token_is_refused_before_decoding() {
    let cursor = Cursor::new(KEY, None).expect("the vector key is 32 bytes");
    let expectation = vectors()[0].expectation();

    // A well-shaped token past the limit: refused on length, before any base64 work.
    let oversized = format!("{}.{}", "A".repeat(MAX_TOKEN_CHARS), "A".repeat(22));
    assert!(oversized.len() > MAX_TOKEN_CHARS);
    let refusal = refusal_of(cursor.verify(&oversized, &expectation));
    assert_eq!(refusal.code, ErrorCode::CURSOR_INVALID);
    assert_eq!(refusal.hint, HINT_INVALID);

    // The limit is a ceiling, not a floor: exactly `MAX_TOKEN_CHARS` characters of junk is refused
    // by the shape and tag checks instead — same code, same hint, no panic either way.
    let at_limit = "A".repeat(MAX_TOKEN_CHARS);
    assert_eq!(
        refusal_of(cursor.verify(&at_limit, &expectation)).code,
        ErrorCode::CURSOR_INVALID
    );
    assert_eq!(
        refusal_of(cursor.verify(&at_limit, &expectation)).hint,
        HINT_INVALID
    );
}

#[test]
fn a_short_key_is_an_internal_error() {
    let short = &KEY.as_bytes()[..CURSOR_KEY_MIN_BYTES - 1];
    let error: AppError = Cursor::new(short, None).expect_err("31 bytes is below the floor");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert_eq!(error.code.as_str(), "internal");
    assert_eq!(
        error.message,
        "cursor key must be at least 32 bytes of key material"
    );
    assert_eq!(
        error.hint.as_deref(),
        Some("set SANGKAR_CURSOR_KEY to a random value of 32 bytes or more")
    );

    // 32 bytes is the floor, and an empty key is the same failure one byte shorter than 31.
    assert!(Cursor::new(KEY, None).is_ok());
    let empty = Cursor::new("", None).expect_err("an empty key is not key material");
    assert_eq!(empty.code, ErrorCode::INTERNAL);
}
