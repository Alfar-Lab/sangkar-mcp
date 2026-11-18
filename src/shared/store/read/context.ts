/**
 * The context every tool handler receives (`TECH-SPEC.md` §6, `plan.constraint_invariant.md`).
 *
 * It is the whole reason a handler is testable without a database and without a server: the
 * reader, the cursor pair, the logger, the clock and the resolved config all arrive as values,
 * so a handler never opens a connection, never reads the environment and never mints an envelope
 * from ambient state. `handle(ctx: ToolContext, args)` is therefore the same signature in every
 * tool, and a fake `ToolContext` built from `#shared/*` alone is a complete unit-test seam.
 *
 * This module imports nothing from `src/tools/**` or `src/index.ts` — deliberately, and asserted
 * by `tests/read-files.test.ts`. A shared module that reached for the composition root would
 * make the reader interface impossible to fake, because importing the root imports every tool.
 *
 * Types only, no runtime: `ToolContext` is an interface, so importing it costs nothing and a
 * handler can be type-checked against a hand-written object literal.
 */
import type { AppConfig } from "#shared/config";
import type { CursorExpectation, CursorPayload, CursorResult } from "#shared/cursor";
import type { Logger } from "#shared/log";
import type { GraphReader } from "#shared/store/read/reader";

/**
 * The cursor seam: sign a page token, verify a resumed one. Structurally identical to
 * `Cursor` from `#shared/cursor.ts`, which is what `createCursors()` returns, so the root can
 * bind the HMAC key once and hand the pair over — a handler can mint and verify a cursor but
 * cannot reach the key behind it.
 *
 * The generation is part of `CursorExpectation`, so expiry is not the handler's decision to
 * make: a token from another generation verifies to `cursor_expired`, never to a wrong page
 * (`DATABASE.md` §8).
 */
export interface CursorFacade {
  sign(payload: CursorPayload): string;
  /** `expected` is `{generation, tool, argsHash}`: what the resuming call claims to be. */
  verify(token: string, expected: CursorExpectation): CursorResult;
}

/**
 * Everything a handler may know about the process. The five fields are the entire seam:
 *
 * - `reader` is the aggregate `GraphReader`; there is no connection anywhere in this type, so a
 *   tool cannot query SQL even by accident;
 * - `cursor` is bound to one key and one shape (`CursorFacade`);
 * - `log` is pre-scoped to the tool name, so a log line is attributable without every call site
 *   naming itself, and nothing is ever written to stdout (that is the MCP protocol);
 * - `now` is injected, so staleness and cursor expiry are testable rather than wall-clock;
 * - `config` is the resolved configuration, so no handler reads the environment.
 */
export interface ToolContext {
  readonly reader: GraphReader;
  readonly cursor: CursorFacade;
  readonly log: Logger;
  readonly now: () => Date;
  readonly config: AppConfig;
}
