import type { Envelope } from "#shared/envelope";

/**
 * The two codes a caller returns itself. Everything an envelope describes maps through
 * `exitCodeOf`; `EXIT_USAGE` is the argv or config refusal that happens before there is an
 * envelope to read, which is why nothing in this module produces it.
 */
export const EXIT_FAILURE = 1;
export const EXIT_USAGE = 2;

/**
 * The one line the CLI writes to stdout: the envelope as compact JSON plus a terminating newline,
 * so a caller that captures stdout gets JSON and nothing else.
 */
export function renderEnvelope(envelope: Envelope<unknown>): string {
  return `${JSON.stringify(envelope)}\n`;
}

/** A payload section read structurally; the renderer sees `unknown`, so nothing is assumed. */
type Fields = Record<string, unknown>;

function fields(value: unknown): Fields | null {
  return typeof value === "object" && value !== null ? (value as Fields) : null;
}

/**
 * A field as text. `null` and `undefined` take the fallback, because that is what `??` did in the
 * human output; anything else is stringified so a number still reaches the line.
 */
function text(source: Fields | null, key: string, fallback: string): string {
  const value = source?.[key];
  return value === undefined || value === null ? fallback : String(value);
}

/**
 * The human `status` output: the status line, then, when the envelope carries a payload, the four
 * detail lines an operator reads. Wording is the `runStatus` wording in `src/index.ts`; this is the
 * same text derived from any envelope instead of the status tool's payload type.
 */
export function statusLines(envelope: Envelope<unknown>): string {
  const lines: string[] = [`${envelope.status}: ${envelope.summary}`];
  const payload = fields(envelope.payload);
  if (payload !== null) {
    const project = fields(payload.project);
    const graph = fields(payload.graph);
    const health = fields(payload.health);
    lines.push(
      `project  ${text(project, "name", "unknown")} (${text(project, "root", "unrecorded")})`,
      `database ${text(project, "db_path", "unknown")}`,
      `graph    ${text(graph, "nodes", "unknown")} nodes, ${text(graph, "edges", "unknown")} edges, ${text(graph, "size_bytes", "unknown")} bytes`,
      `health   ${text(health, "indexer", "unknown")}, ${text(health, "consecutive_failures", "unknown")} consecutive failures`,
    );
  }
  return `${lines.join("\n")}\n`;
}

/** The process code an envelope earns: a positive or negative answer is 0, the two failures are not. */
export function exitCodeOf(envelope: Envelope<unknown>): number {
  return envelope.status === "error" || envelope.status === "bootstrapping" ? EXIT_FAILURE : 0;
}
