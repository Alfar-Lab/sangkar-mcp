import { describe, expect, test } from "bun:test";
import {
  EXIT_FAILURE,
  EXIT_USAGE,
  exitCodeOf,
  renderEnvelope,
  statusLines,
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

/** The payload `status` produces, field for field, so the wording can be compared as text. */
const STATUS_PAYLOAD = {
  project: {
    name: "codegraph-mcp",
    root: "/src/codegraph-mcp",
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

/** The `status` tool's own summary shape, so the first line is compared against real wording. */
const SUMMARY = "530 files indexed, generation g-7f3a91. Whole repository.";

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

describe("renderEnvelope: one line of JSON", () => {
  test("the output parses back to the envelope it was given", () => {
    const envelope = statusEnvelope();
    expect(JSON.parse(renderEnvelope(envelope))).toEqual(envelope);
  });

  test("the only newline is the trailing one", () => {
    const output = renderEnvelope(statusEnvelope());
    expect(output.endsWith("\n")).toBe(true);
    expect(output.slice(0, -1).includes("\n")).toBe(false);
  });

  test("an envelope with no payload still renders compact JSON", () => {
    const envelope = fail(new AppError("internal", "store refused"), spec("store refused"));
    const output = renderEnvelope(envelope);
    expect(output).toBe(`${JSON.stringify(envelope)}\n`);
    expect(JSON.parse(output)).toEqual(envelope);
  });
});

describe("statusLines: the runStatus wording", () => {
  test("a payload renders five lines in the operator wording", () => {
    expect(statusLines(statusEnvelope())).toBe(
      [
        `ok: ${SUMMARY}`,
        `project  codegraph-mcp (/src/codegraph-mcp)`,
        `database /data/graph.db`,
        `graph    530 nodes, 1188 edges, 1234567 bytes`,
        `health   idle, 0 consecutive failures`,
        ``,
      ].join("\n"),
    );
  });

  test("the block ends with one newline and carries no other", () => {
    const lines = statusLines(statusEnvelope());
    expect(lines.split("\n")).toHaveLength(6);
    expect(lines.endsWith("\n")).toBe(true);
  });

  test("a null payload prints the status line alone", () => {
    for (const status of ["error", "bootstrapping"]) {
      const envelope = envelopeFor(status);
      const lines = statusLines(envelope);
      expect(lines).toBe(`${status}: ${envelope.summary}\n`);
      expect(lines.split("\n").filter((line) => line !== "")).toHaveLength(1);
    }
  });

  test("a missing name, root and indexer fall back to unknown and unrecorded", () => {
    const payload = {
      project: {
        name: null,
        root: null,
        commit: null,
        db_path: "/data/graph.db",
      },
      graph: { nodes: 1, edges: 0, size_bytes: 4096, db_path: "/data/graph.db" },
      health: {
        indexer: null,
        last_publish_ms: 0,
        consecutive_failures: 3,
        statement_cache_entries: 0,
      },
    };
    expect(statusLines(ok(payload, spec(SUMMARY)))).toBe(
      [
        `ok: ${SUMMARY}`,
        `project  unknown (unrecorded)`,
        `database /data/graph.db`,
        `graph    1 nodes, 0 edges, 4096 bytes`,
        `health   unknown, 3 consecutive failures`,
        ``,
      ].join("\n"),
    );
  });

  test("a payload whose sections are absent keeps the four lines with fallbacks", () => {
    // the renderer narrows from `unknown`, so a payload of the wrong shape must not throw
    expect(statusLines(ok({}, spec(SUMMARY)))).toBe(
      [
        `ok: ${SUMMARY}`,
        `project  unknown (unrecorded)`,
        `database unknown`,
        `graph    unknown nodes, unknown edges, unknown bytes`,
        `health   unknown, unknown consecutive failures`,
        ``,
      ].join("\n"),
    );
  });

  test("every status names itself on the first line", () => {
    for (const [status, envelope] of allStatuses()) {
      expect(statusLines(envelope).split("\n")[0]).toBe(`${status}: ${envelope.summary}`);
    }
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
