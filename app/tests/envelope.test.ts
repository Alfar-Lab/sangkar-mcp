import { describe, expect, test } from "bun:test";
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
  relationFor,
} from "#shared/envelope";
import { AppError } from "#shared/errors";

const REQUIRED_FIELDS = [
  "status",
  "summary",
  "read_status",
  "coverage",
  "totals",
  "truncated",
  "truncation_reason",
  "next",
  "next_tools",
] as const;

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

function baseSpec(): EnvelopeSpec {
  return {
    summary: "resolve_bare_call_targets: 2 callers, 15 callees (4 shown)",
    read_status: { ...FRESH },
    coverage: { ...CLEAN, not_indexed: [] },
  };
}

const CONSTRUCTORS: readonly [string, () => Envelope<unknown>][] = [
  ["ok", () => ok({ callers: [{ id: 3211 }] }, baseSpec())],
  [
    "ambiguous",
    () => ambiguous({ candidates: [{ qn: "a.py::A.f" }, { qn: "a.py::A.g" }] }, baseSpec()),
  ],
  ["notFound", () => notFound({ near_misses: [] }, baseSpec())],
  ["fail", () => fail(new AppError("not_found", "no such symbol"), baseSpec())],
  [
    "bootstrapping",
    () => {
      const spec = baseSpec();
      return bootstrapping({ ...spec, retry_after_ms: 1500 });
    },
  ],
];

function missingFields(env: Envelope<unknown>): string[] {
  const record = env as unknown as Record<string, unknown>;
  const missing: string[] = REQUIRED_FIELDS.filter(
    (field) => !Object.hasOwn(record, field) || record[field] === undefined,
  );
  if (!Object.hasOwn(record, "payload") || record.payload === undefined) {
    missing.push("payload");
  }
  return missing;
}

describe("envelope: nine required fields", () => {
  test("every constructor returns all nine fields plus payload", () => {
    for (const [name, build] of CONSTRUCTORS) {
      expect({ constructor: name, missing: missingFields(build()) }).toEqual({
        constructor: name,
        missing: [],
      });
    }
  });

  test("every constructor sets the status it is named after", () => {
    expect(CONSTRUCTORS.map(([name, build]) => [name, build().status])).toEqual([
      ["ok", "ok"],
      ["ambiguous", "ambiguous"],
      ["notFound", "not_found"],
      ["fail", "error"],
      ["bootstrapping", "bootstrapping"],
    ]);
  });

  test("totals and next_tools are never omitted, even when the caller supplies neither", () => {
    const env = ok({}, baseSpec());
    expect(env.totals).toBeDefined();
    expect(env.next_tools).toEqual([]);
    expect(env.truncated).toBe(false);
    expect(env.truncation_reason).toBeNull();
    expect(env.next).toBeNull();
  });
});

describe("envelope: error is present if and only if status is error or bootstrapping", () => {
  const withError = new Set(["fail", "bootstrapping"]);

  test("presence of the error object matches the status", () => {
    const observed = CONSTRUCTORS.map(([name, build]) => [name, Object.hasOwn(build(), "error")]);
    expect(observed).toEqual(CONSTRUCTORS.map(([name]) => [name, withError.has(name)]));
  });

  test("error and bootstrapping carry a contract error payload", () => {
    const spec = baseSpec();
    const failed = fail(
      new AppError("cursor_expired", "cursor is from an older generation", {
        hint: "re-run the original call",
      }),
      spec,
    );
    expect(failed.error).toEqual({
      code: "cursor_expired",
      message: "cursor is from an older generation",
      hint: "re-run the original call",
    });

    const building = bootstrapping({ ...baseSpec(), retry_after_ms: 2000, hint: "call status" });
    expect(building.error?.code).toBe("bootstrapping");
    expect(building.error?.retry_after_ms).toBe(2000);
    expect(building.error?.hint).toBe("call status");
    expect(building.error?.message.length).toBeGreaterThan(0);
  });

  test("fail normalises an AppError, a plain payload and an unknown throw", () => {
    expect(
      fail(
        new AppError("index_locked", "worker holds the lock", {
          retryable: true,
          retryAfterMs: 750,
        }),
        baseSpec(),
      ).error,
    ).toEqual({
      code: "index_locked",
      message: "worker holds the lock",
      retryable: true,
      retry_after_ms: 750,
    });
    expect(
      fail({ code: "not_found", message: "no node", hint: "search first" }, baseSpec()).error,
    ).toEqual({
      code: "not_found",
      message: "no node",
      hint: "search first",
    });
    expect(fail(new Error("boom"), baseSpec()).error).toEqual({
      code: "internal",
      message: "boom",
    });
    expect(fail("boom", baseSpec()).error).toEqual({ code: "internal", message: "boom" });
  });

  test("fail refuses a code outside the closed union instead of forwarding it", () => {
    expect(fail({ code: "root_mismatch", message: "not a real code" }, baseSpec()).error).toEqual({
      code: "internal",
      message: "not a real code",
    });
  });
});

describe("envelope: read_status", () => {
  test("optional members are absent, never present as undefined", () => {
    const env = ok({}, baseSpec());
    expect(Object.hasOwn(env.read_status, "stale_reason")).toBe(false);
    expect(Object.hasOwn(env.read_status, "stale_checked_at")).toBe(false);
  });

  test("a stale status keeps its reason and its check timestamp", () => {
    const spec = baseSpec();
    spec.read_status = {
      generation: "g-7f3a91",
      indexed_at: "2026-11-17T13:50:15Z",
      stale: true,
      stale_reason: "3 files changed since index",
      stale_checked_at: "2026-11-17T14:02:00Z",
    };
    const env = ok({}, spec);
    expect(env.read_status.stale).toBe(true);
    expect(env.read_status.stale_reason).toBe("3 files changed since index");
    expect(env.read_status.stale_checked_at).toBe("2026-11-17T14:02:00Z");
  });
});

describe("envelope: totals and the eq/gte rule", () => {
  test("the default relation is eq only for clean, fresh coverage", () => {
    expect(relationFor(FRESH, CLEAN)).toBe("eq");
    expect(relationFor({ ...FRESH, stale: true }, CLEAN)).toBe("gte");
    expect(relationFor(FRESH, { ...CLEAN, complete: false })).toBe("gte");
    expect(relationFor(FRESH, { ...CLEAN, unresolved_edges: 321 })).toBe("gte");
  });

  test("constructors derive the default relation and keep caller-supplied totals", () => {
    expect(ok({}, baseSpec()).totals).toEqual({ total_relation: "eq" });

    const stale = baseSpec();
    stale.read_status = { ...FRESH, stale: true, stale_reason: "3 files changed since index" };
    expect(ok({}, stale).totals).toEqual({ total_relation: "gte" });

    const partial = baseSpec();
    partial.coverage = { ...CLEAN, complete: false, skipped: 12, failed: 3, unresolved_edges: 321 };
    expect(ok({}, partial).totals).toEqual({ total_relation: "gte" });

    const counted = baseSpec();
    counted.totals = { callers_total: 2, callees_total: 15, total_relation: "gte" };
    expect(ok({}, counted).totals).toEqual({
      callers_total: 2,
      callees_total: 15,
      total_relation: "gte",
    });
  });

  test("coverage carries every declared count", () => {
    const spec = baseSpec();
    spec.coverage = {
      ...CLEAN,
      not_indexed: [{ path: "docs/legacy.xslt", reason: "grammar_unavailable" }],
    };
    const env = ok({}, spec);
    expect(env.coverage).toEqual({
      complete: true,
      indexed: 530,
      skipped: 0,
      failed: 0,
      unresolved_edges: 0,
      not_indexed: [{ path: "docs/legacy.xslt", reason: "grammar_unavailable" }],
    });
  });
});

describe("envelope: defensive copies and overrides", () => {
  test("mutating the caller's arrays after construction does not change the envelope", () => {
    const notIndexed = [{ path: "docs/legacy.xslt", reason: "grammar_unavailable" }];
    const nextTools = ["expand"];
    const spec = baseSpec();
    spec.coverage = { ...CLEAN, not_indexed: notIndexed };
    spec.next_tools = nextTools;
    spec.truncated = true;
    spec.truncation_reason = "budget";
    spec.next = "cur_8f21ad";

    const env = ok({ rows: [] }, spec);
    notIndexed.push({ path: "other.py", reason: "ignored" });
    nextTools.push("impact");

    expect(env.coverage.not_indexed).toHaveLength(1);
    expect(env.next_tools).toEqual(["expand"]);
    expect(env.truncated).toBe(true);
    expect(env.truncation_reason).toBe("budget");
    expect(env.next).toBe("cur_8f21ad");
  });
});
