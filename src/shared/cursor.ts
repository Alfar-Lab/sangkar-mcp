import { createHmac, timingSafeEqual } from "node:crypto";
import { AppError } from "#shared/errors";

/**
 * Stateless, signed pagination cursors (DATABASE.md §8). No cursor table, no server memory:
 * the token *is* the state, so a page boundary survives a restart and a stale token is refused
 * instead of silently addressing a different row.
 *
 * The key is a parameter, never read from the environment — `#shared/config.ts` is the only
 * file allowed to touch it and hands `AppConfig.cursorKey` to `createCursors()`.
 *
 * Layout: `base64url(payload) "." base64url(tag)` where `tag` is the first 16 bytes of
 * HMAC-SHA256(key, base64url(payload)). MAC over the encoded segment rather than the JSON keeps
 * verification a single pass over the bytes that arrived.
 */

/**
 * Minimum key material. An HMAC-SHA256 key shorter than the hash's 256-bit output buys nothing
 * and is the usual way a "secret" becomes guessable, so a short key is a startup failure.
 */
export const CURSOR_KEY_MIN_BYTES = 32;

/** Truncated tag width: 128 bits, the standard MAC truncation floor, and a shorter token. */
export const CURSOR_TAG_BYTES = 16;

/**
 * Refuse absurd client input before decoding it. Real tokens are ~150 characters; a cursor is
 * untrusted input, so its size is bounded like any other.
 */
const MAX_TOKEN_CHARS = 1024;

/** DESIGN.md §7.2 fixes this wording: an expired cursor tells the agent to re-run, not to guess. */
const HINT_RE_RUN = "re-run the original call";
const HINT_INVALID = "re-run the original call to mint a fresh cursor";
const HINT_MISMATCH = "re-run the original call with the same tool and arguments";

/** What a cursor pins: the generation it was minted in, the call it belongs to, and the offset. */
export interface CursorPayload {
  generation: string;
  tool: string;
  argsHash: string;
  /** Items already delivered; an index into the generation's stable ordering. */
  position: number;
}

/** What the resuming call claims. Anything outside this must not be served. */
export interface CursorExpectation {
  generation: string;
  tool: string;
  argsHash: string;
}

/** The three refusals of `errors.ts`'s union a cursor can produce (DATABASE.md §8). */
export type CursorRefusalCode = "cursor_invalid" | "cursor_expired" | "cursor_mismatch";

/** A cursor that verified: the position it addressed. */
export interface CursorAcceptance {
  ok: true;
  position: number;
}

/** A cursor that did not verify. `hint` is always present, so a caller cannot drop it. */
export interface CursorRefusal {
  ok: false;
  code: CursorRefusalCode;
  hint: string;
}

export type CursorResult = CursorAcceptance | CursorRefusal;

/**
 * The cursor seam a tool receives in `ToolContext.cursor` (TECH-SPEC.md §6): bound to one key,
 * so no handler ever holds the HMAC secret.
 */
export interface Cursor {
  sign(payload: CursorPayload): string;
  verify(token: string, expected: CursorExpectation): CursorResult;
}

function assertKey(key: string): void {
  if (typeof key !== "string" || Buffer.byteLength(key, "utf8") < CURSOR_KEY_MIN_BYTES) {
    throw new AppError(
      "internal",
      `cursor key must be at least ${CURSOR_KEY_MIN_BYTES} bytes of key material`,
      { hint: "set SANGKAR_CURSOR_KEY to a random value of 32 bytes or more" },
    );
  }
}

function nonEmpty(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}

function isPosition(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function isPayload(value: unknown): value is CursorPayload {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  const candidate = value as Partial<CursorPayload>;
  return (
    nonEmpty(candidate.generation) &&
    nonEmpty(candidate.tool) &&
    nonEmpty(candidate.argsHash) &&
    isPosition(candidate.position)
  );
}

function tagFor(key: string, body: string): Buffer {
  return createHmac("sha256", key).update(body).digest().subarray(0, CURSOR_TAG_BYTES);
}

function refusal(code: CursorRefusalCode, hint: string): CursorRefusal {
  return { ok: false, code, hint };
}

/**
 * Mints a cursor. Signing is deterministic for a given `(payload, key)` — there is no issued-at
 * field, because expiry is generation-bound (DATABASE.md §8) and a timestamp nothing reads would
 * only make the token un-reproducible in tests.
 *
 * Throws `AppError("internal")` for a short key or a malformed payload: both are server bugs,
 * never client input.
 */
export function sign(payload: CursorPayload, key: string): string {
  assertKey(key);
  if (!isPayload(payload)) {
    throw new AppError("internal", "invalid cursor payload", {
      hint: "generation, tool and argsHash must be non-empty strings; position a non-negative integer",
    });
  }
  const body = Buffer.from(JSON.stringify(payload), "utf8").toString("base64url");
  return `${body}.${tagFor(key, body).toString("base64url")}`;
}

/**
 * Verifies a cursor in the order DATABASE.md §8 fixes: MAC, then generation, then call identity.
 * The order is load-bearing — a token from an older generation must read as `cursor_expired`
 * (re-run) rather than `cursor_mismatch`, and a tampered token as `cursor_invalid` rather than
 * either.
 */
export function verify(token: string, expected: CursorExpectation, key: string): CursorResult {
  assertKey(key);
  if (typeof token !== "string" || token.length === 0 || token.length > MAX_TOKEN_CHARS) {
    return refusal("cursor_invalid", HINT_INVALID);
  }
  const dot = token.indexOf(".");
  if (dot <= 0 || dot === token.length - 1 || token.indexOf(".", dot + 1) !== -1) {
    return refusal("cursor_invalid", HINT_INVALID);
  }

  const body = token.slice(0, dot);
  const tag = Buffer.from(token.slice(dot + 1), "base64url");
  // A wrong-length tag cannot equal a real one; checking first keeps `timingSafeEqual` from
  // throwing on mismatched lengths.
  if (tag.length !== CURSOR_TAG_BYTES || !timingSafeEqual(tag, tagFor(key, body))) {
    return refusal("cursor_invalid", HINT_INVALID);
  }

  let decoded: unknown;
  try {
    decoded = JSON.parse(Buffer.from(body, "base64url").toString("utf8"));
  } catch {
    // Only a key holder can reach this; a signed-but-unparseable payload is still not a page.
    return refusal("cursor_invalid", HINT_INVALID);
  }
  if (!isPayload(decoded)) {
    return refusal("cursor_invalid", HINT_INVALID);
  }

  if (decoded.generation !== expected.generation) {
    return refusal("cursor_expired", HINT_RE_RUN);
  }
  if (decoded.tool !== expected.tool || decoded.argsHash !== expected.argsHash) {
    return refusal("cursor_mismatch", HINT_MISMATCH);
  }
  return { ok: true, position: decoded.position };
}

/**
 * Binds the key once, at the composition root, and returns the `{sign, verify}` pair a
 * `ToolContext` carries. The key length check happens here so a misconfigured server fails at
 * startup rather than at the first truncation.
 */
export function createCursors(options: { key: string }): Cursor {
  const { key } = options;
  assertKey(key);
  return {
    sign: (payload) => sign(payload, key),
    verify: (token, expected) => verify(token, expected, key),
  };
}
