import { encode } from "@toon-format/toon";
import type { Envelope } from "#shared/envelope";

/**
 * The default TOON rendering, the `--json` JSON rendering, and the exit-code mapping: everything the
 * two surfaces say about an envelope once a handler has produced it, in one place so the CLI and the
 * MCP answer cannot disagree about the bytes. TOON is the default because an agent pays for the tokens
 * of every answer; `--json` stays one-line JSON for a caller that parses stdout.
 */

/**
 * The two codes a caller returns itself. Everything an envelope describes maps through
 * `exitCodeOf`; `EXIT_USAGE` is the argv or config refusal that happens before there is an
 * envelope to read, which is why nothing in this module produces it.
 */
export const EXIT_FAILURE = 1;
export const EXIT_USAGE = 2;

/**
 * The envelope as one line of compact JSON plus a terminating newline: the `--json` rendering, and
 * the shape a caller that captures stdout can parse without knowing TOON.
 */
export function renderJson(envelope: Envelope<unknown>): string {
  return `${JSON.stringify(envelope)}\n`;
}

/**
 * The default rendering: the envelope as TOON (`Token-Oriented Object Notation`) plus a terminating
 * newline. TOON spells the same JSON data model in fewer tokens, which is what an agent pays for on
 * every answer; `decode` turns it back into the identical value, so nothing is lost by not being
 * JSON.
 */
export function renderToon(envelope: Envelope<unknown>): string {
  return `${encode(envelope)}\n`;
}

/**
 * One envelope, in whichever of the two forms the caller asked for. The only place a CLI rendering
 * is chosen, so `--json` cannot mean one thing for the tool commands and another for `mcp`.
 */
export function renderOutput(
  envelope: Envelope<unknown>,
  opts: { readonly json: boolean },
): string {
  return opts.json ? renderJson(envelope) : renderToon(envelope);
}

/**
 * The MCP `tools/call` text: the same choice without the trailing newline, because a protocol result
 * is a string field rather than a line stream and the JSON it replaces never ended in one. Encoding
 * lives here so this file holds the only `encode` call in the app.
 */
export function mcpText(envelope: Envelope<unknown>, outputJson: boolean): string {
  return outputJson ? JSON.stringify(envelope) : encode(envelope);
}

/** The process code an envelope earns: a positive or negative answer is 0, the two failures are not. */
export function exitCodeOf(envelope: Envelope<unknown>): number {
  return envelope.status === "error" || envelope.status === "bootstrapping" ? EXIT_FAILURE : 0;
}
