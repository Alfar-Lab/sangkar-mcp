import { describe, expect, test } from "bun:test";
import { decode, type JsonValue } from "@toon-format/toon";
import {
  EXIT_FAILURE,
  EXIT_USAGE,
  exitCodeOf,
  mcpText,
  renderJson,
  renderOutput,
  renderToon,
} from "#shared/cli/render";
import {
  ambiguous,
  bootstrapping,
  type Coverage,
  type Envelope,
  type EnvelopeSpec,
  fail,
  notFound,
  ok,
  type ReadStatus,
} from "#shared/envelope";
import { AppError } from "#shared/errors";

const FRESH: ReadStatus = {
  generation: "g-7f3a91",
  indexed_at: "2026-11-17T13:50:15Z",
  stale: false,
};

const CLEAN: Coverage = {
  complete: true,
  indexed: 530,
  skipped: 0,
  failed: 0,
  unresolved_edges: 0,
  not_indexed: [],
};

function spec(summary: string): EnvelopeSpec {
  return { summary, read_status: { ...FRESH }, coverage: { ...CLEAN, not_indexed: [] } };
}

/** The payload `status` produces, field for field, so the encoding is proven on a real shape. */
const STATUS_PAYLOAD = {
  project: {
    name: "sangkar-mcp",
    root: "/src/sangkar-mcp",
    commit: "ce28fe2",
    db_path: "/data/graph.db",
  },
  graph: { nodes: 530, edges: 1188, size_bytes: 1234567, db_path: "/data/graph.db" },
  languages: [{ name: "typescript", files: 41, symbols: 530 }],
  paths: [],
  health: {
    indexer: "idle",
    last_publish_ms: 1200,
    consecutive_failures: 0,
    statement_cache_entries: 3,
  },
};

/** The `status` tool's own summary shape, so the encoding is proven on real wording. */
const SUMMARY = "530 files indexed, generation g-7f3a91. Whole repository.";

/** A payload whose content is the worst case for a line- and comma-oriented notation. */
const RAW_SOURCE = `export function f(a, b) { return [a, "b"] | c; }
// the second line carries a newline into the value`;

/** Two fields, one row per item:  the shape TOON writes as a table. */
const UNIFORM_SKIP_LIST = [
  { path: "src/a.ts", reason: "not indexed" },
  { path: "src/b.ts", reason: `skipped, "quoted"` },
];

/** A cursor as the tools spell it: opaque, base64url, and never quoted by the encoder. */
const CURSOR = "eyJyZXBvIjoiL3NyYyIsIm9mZiI6MjB9";

/** Every character class an encoder has to escape, plus nulls and an empty object. */
const ADVERSARIAL_PAYLOAD = {
  results: UNIFORM_SKIP_LIST,
  source: RAW_SOURCE,
  separators: 'a, b | c { brace } [ bracket ] "quoted"',
  skipped: [null, { path: "src/c.ts", reason: "binary" }, null],
  empty: {},
  summary: "x".repeat(600),
};

function statusEnvelope(): Envelope<typeof STATUS_PAYLOAD> {
  return ok(STATUS_PAYLOAD, spec(SUMMARY));
}

/** `bootstrapping` needs its retry window; everything else builds through the plain constructors. */
function allStatuses(): readonly [string, Envelope<unknown>][] {
  return [
    ["ok", statusEnvelope()],
    ["ambiguous", ambiguous({ candidates: [] }, spec("two matches"))],
    ["not_found", notFound({ near_misses: [] }, spec("no such symbol"))],
    ["error", fail(new AppError("internal", "store refused"), spec("store refused"))],
    ["bootstrapping", bootstrapping({ ...spec("indexing"), retry_after_ms: 900 })],
  ];
}

function envelopeFor(status: string): Envelope<unknown> {
  const found = allStatuses().find(([name]) => name === status);
  if (found === undefined) {
    throw new Error(`no fixture for ${status}`);
  }
  return found[1];
}

/**
 * The body of a rendering, with its one trailing newline proven first: a line stream that ended in
 * two newlines would hand `decode` an empty last line, and a caller reading stdout line by line
 * would see a blank answer where there is none.
 */
function bodyOf(rendered: string): string {
  expect(rendered.endsWith("\n")).toBe(true);
  expect(rendered.slice(0, -1).endsWith("\n")).toBe(false);
  return rendered.slice(0, -1);
}

/** What a consumer does with the default rendering: read it back as the same value. */
function decoded(rendered: string): JsonValue {
  return decode(bodyOf(rendered));
}

/**
 * The same, read as the envelope it was rendered from. TOON carries the JSON data model, so the
 * cast stands for the losslessness the round-trip cases assert rather than for any leniency here.
 */
function decodedEnvelope<T>(rendered: string): Envelope<T> {
  return decode(bodyOf(rendered)) as unknown as Envelope<T>;
}

/** An envelope, typed as the value `decode` speaks: what a round trip is compared against. */
function asJson(value: unknown): JsonValue {
  return value as JsonValue;
}

describe("renderJson: one line of JSON", () => {
  test("the output parses back to the envelope it was given", () => {
    const envelope = statusEnvelope();
    expect(JSON.parse(renderJson(envelope))).toEqual(envelope);
  });

  test("the only newline is the trailing one", () => {
    bodyOf(renderJson(statusEnvelope()));
  });

  test("an envelope with no payload still renders compact JSON", () => {
    const envelope = fail(new AppError("internal", "store refused"), spec("store refused"));
    const output = renderJson(envelope);
    expect(output).toBe(`${JSON.stringify(envelope)}\n`);
    expect(JSON.parse(output)).toEqual(envelope);
  });
});

describe("renderToon: the default rendering", () => {
  test("a status envelope decodes back to the value it was given", () => {
    const envelope = statusEnvelope();
    expect(decoded(renderToon(envelope))).toEqual(asJson(envelope));
  });

  test("the only newline is the trailing one, whatever the payload holds", () => {
    // A newline inside a value is escaped, so the line count of the rendering stays one answer.
    bodyOf(renderToon(ok(ADVERSARIAL_PAYLOAD, spec(SUMMARY))));
  });

  test("adversarial content survives the round trip unchanged", () => {
    const envelope = ok(ADVERSARIAL_PAYLOAD, { ...spec(SUMMARY), next: CURSOR });
    expect(decoded(renderToon(envelope))).toEqual(asJson(envelope));
  });

  test("a null element, a null payload and an empty payload all round trip", () => {
    // Three absences the envelope contract distinguishes: "no elements that are null", "no item at
    // all" and "an item whose fields are all absent".
    const withNulls = ok({ items: [null, { path: "a.ts" }, null] }, spec(SUMMARY));
    expect(decoded(renderToon(withNulls))).toEqual(asJson(withNulls));

    const noPayload = fail(new AppError("internal", "store refused"), spec("store refused"));
    expect(noPayload.payload).toBeNull();
    expect(decoded(renderToon(noPayload))).toEqual(asJson(noPayload));

    const emptyPayload = ok({}, spec(SUMMARY));
    expect(decoded(renderToon(emptyPayload))).toEqual(asJson(emptyPayload));
  });

  test("a long summary is not wrapped into a second line", () => {
    const envelope = ok({}, spec("y".repeat(2_000)));
    expect(decodedEnvelope(renderToon(envelope)).summary).toBe("y".repeat(2_000));
    bodyOf(renderToon(envelope));
  });

  test("a uniform array renders as one table header and one row per item", () => {
    const rendered = bodyOf(renderToon(ok({ results: UNIFORM_SKIP_LIST }, spec(SUMMARY))));
    const lines = rendered.split("\n");
    // The table form is what makes a uniform list cheap: the field names appear once, not per row.
    const header = lines.filter((line) => /^\s+results\[2\]\{path,reason\}:$/.test(line));
    expect(header).toHaveLength(1);
    const indent = (header[0]?.length ?? 0) - (header[0]?.trimStart().length ?? 0);
    // One row per item, indented under the header, and the comma inside the second row's reason is
    // quoted rather than splitting that row into a third cell.
    const rows = lines.filter((line) => /^\s+src\//.test(line));
    expect(rows).toHaveLength(2);
    for (const row of rows) {
      expect(row.length - row.trimStart().length).toBeGreaterThan(indent);
    }
    expect(rendered).toContain("src/a.ts,not indexed");
  });

  test("every status encodes and decodes to itself", () => {
    for (const [status, envelope] of allStatuses()) {
      expect({ status, got: decoded(renderToon(envelope)) }).toEqual({
        status,
        got: asJson(envelope),
      });
    }
  });
});

describe("renderOutput: one envelope, two renderings", () => {
  test("the flag picks the form", () => {
    const envelope = statusEnvelope();
    expect(renderOutput(envelope, { json: true })).toBe(renderJson(envelope));
    expect(renderOutput(envelope, { json: false })).toBe(renderToon(envelope));
  });

  test("both forms carry the same envelope", () => {
    const envelope = statusEnvelope();
    expect(decoded(renderOutput(envelope, { json: false }))).toEqual(
      JSON.parse(bodyOf(renderOutput(envelope, { json: true }))),
    );
  });
});

describe("mcpText: the protocol result, no trailing newline", () => {
  test("the default text is TOON and parses to the envelope", () => {
    const envelope = statusEnvelope();
    const text = mcpText(envelope, false);
    // A protocol result is one string field, not a line stream: the newline renderToon adds would
    // be part of the answer text.
    expect(text.endsWith("\n")).toBe(false);
    expect(decode(text)).toEqual(asJson(envelope));
  });

  test("--json makes the text JSON, still without a newline", () => {
    const envelope = statusEnvelope();
    const text = mcpText(envelope, true);
    expect(text.endsWith("\n")).toBe(false);
    expect(text).toBe(JSON.stringify(envelope));
    expect(JSON.parse(text)).toEqual(asJson(envelope));
  });
});

describe("exitCodeOf: five statuses, two codes", () => {
  test("ok, ambiguous and not_found exit 0", () => {
    for (const status of ["ok", "ambiguous", "not_found"]) {
      expect(exitCodeOf(envelopeFor(status))).toBe(0);
    }
  });

  test("error and bootstrapping exit EXIT_FAILURE", () => {
    for (const status of ["error", "bootstrapping"]) {
      expect(exitCodeOf(envelopeFor(status))).toBe(EXIT_FAILURE);
    }
  });

  test("the codes are the documented pair", () => {
    expect(EXIT_FAILURE).toBe(1);
    expect(EXIT_USAGE).toBe(2);
  });
});
