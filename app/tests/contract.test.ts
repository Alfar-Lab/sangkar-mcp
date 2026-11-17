import { describe, expect, test } from "bun:test";
import { BYTES_PER_TOKEN, emit } from "#shared/budget";
import { type CursorRefusalCode, type CursorResult, createCursors } from "#shared/cursor";
import {
  ambiguous,
  bootstrapping,
  type Coverage,
  ENVELOPE_STATUSES,
  type Envelope,
  type EnvelopeSpec,
  type EnvelopeStatus,
  fail,
  notFound,
  ok,
  type ReadStatus,
} from "#shared/envelope";
import { AppError } from "#shared/errors";
import { isUnderPrefix, prefixBounds } from "#shared/paths";

/**
 * The phase 0 contract gate. Every assertion here restates a rule from `plan.constraint_invariant.md`
 * that later phases build on, and each one is checked against the shipped constructors rather than
 * against a fixture that happens to agree with them: the nine fields of the envelope, the
 * conditional `error`, the budget ceiling with its `eq` → `gte` downgrade, the two cursor
 * refusals, and the prefix rule that keeps `src-old` out of `src`.
 *
 * Where a rule has a boundary, the boundary is measured and stated instead of being assumed away:
 * `emit` cannot cut below the envelope's own size, so the budget tests separate "the payload was
 * cut to fit" from "the payload has nothing left to cut".
 */

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

/** Complete coverage, no unresolved edges, fresh generation: the only state that may claim `eq`. */
const CLEAN: Coverage = {
  complete: true,
  indexed: 530,
  skipped: 0,
  failed: 0,
  unresolved_edges: 0,
  not_indexed: [],
};

const KEY = "contract-gate-key-0123456789abcdef-0123";
const cursors = createCursors({ key: KEY });
const CURSOR_CALL = {
  generation: FRESH.generation,
  tool: "trace",
  argsHash: "sha256:1111111111111111111111111111111111111111111111111111111111111111",
} as const;

function baseSpec(): EnvelopeSpec {
  return {
    summary: "resolve: 1 definition",
    read_status: { ...FRESH },
    coverage: { ...CLEAN, not_indexed: [] },
  };
}

/** A `next` token the size of a real one, so the reserve inside `emit` is exercised honestly. */
function mint(position: number): string {
  return cursors.sign({ ...CURSOR_CALL, position });
}

const MINTED_TOKEN_BYTES = Buffer.byteLength(mint(1), "utf8");

function bytes(value: unknown): number {
  return Buffer.byteLength(JSON.stringify(value), "utf8");
}

interface Constructor {
  readonly name: string;
  readonly status: EnvelopeStatus;
  readonly build: () => Envelope<unknown>;
}

const CONSTRUCTORS: readonly Constructor[] = [
  { name: "ok", status: "ok", build: () => ok({ definitions: [{ id: 7 }] }, baseSpec()) },
  {
    name: "ambiguous",
    status: "ambiguous",
    build: () =>
      ambiguous({ candidates: [{ qn: "src/a.ts::A.run" }, { qn: "src/b.ts::B.run" }] }, baseSpec()),
  },
  { name: "notFound", status: "not_found", build: () => notFound({ near_misses: [] }, baseSpec()) },
  {
    name: "fail",
    status: "error",
    build: () => fail(new AppError("not_found", "no such symbol"), baseSpec()),
  },
  {
    name: "bootstrapping",
    status: "bootstrapping",
    build: () => bootstrapping({ ...baseSpec(), retry_after_ms: 1500 }),
  },
];

/** The only two statuses whose answer is a failure, and therefore the only two carrying `error`. */
const STATUSES_CARRYING_ERROR: readonly EnvelopeStatus[] = ["error", "bootstrapping"];

describe("envelope: nine fields, conditional error", () => {
  test("every constructor returns all nine fields, none of them undefined", () => {
    const violations: string[] = [];

    for (const builder of CONSTRUCTORS) {
      const env = builder.build();
      if (env.status !== builder.status) {
        violations.push(`${builder.name}: status ${env.status} !== ${builder.status}`);
      }
      if (!(ENVELOPE_STATUSES as readonly string[]).includes(env.status)) {
        violations.push(`${builder.name}: status ${env.status} is outside ENVELOPE_STATUSES`);
      }
      for (const field of REQUIRED_FIELDS) {
        if (!Object.hasOwn(env, field)) {
          violations.push(`${builder.name}: ${field} is missing`);
          continue;
        }
        if (env[field] === undefined) {
          violations.push(`${builder.name}: ${field} is present but undefined`);
        }
      }
      if (!Array.isArray(env.next_tools)) {
        violations.push(`${builder.name}: next_tools is not an array`);
      }
      if (env.truncation_reason !== null && typeof env.truncation_reason !== "string") {
        violations.push(`${builder.name}: truncation_reason is neither null nor a string`);
      }
      if (env.next !== null && typeof env.next !== "string") {
        violations.push(`${builder.name}: next is neither null nor a string`);
      }
      if (!Array.isArray(env.coverage.not_indexed)) {
        violations.push(`${builder.name}: coverage.not_indexed is not an array`);
      }
      if (typeof env.totals.total_relation !== "string") {
        violations.push(`${builder.name}: totals.total_relation is not a string`);
      }
    }

    expect(violations).toEqual([]);
  });

  test("error is present if and only if the status is error or bootstrapping", () => {
    const violations: string[] = [];

    for (const builder of CONSTRUCTORS) {
      const env = builder.build();
      const expected = STATUSES_CARRYING_ERROR.includes(env.status);
      const present = Object.hasOwn(env, "error");
      if (present !== expected) {
        violations.push(
          `${builder.name}: error ${present ? "present" : "absent"} but status ${env.status}`,
        );
      }
      if (present && (env.error === undefined || typeof env.error.code !== "string")) {
        violations.push(`${builder.name}: error is present without a contract code`);
      }
    }

    expect(violations).toEqual([]);
    // A positive answer may not smuggle a failure in through an optional field.
    expect(Object.hasOwn(ok({}, baseSpec()), "error")).toBe(false);
    expect(Object.hasOwn(notFound({}, baseSpec()), "error")).toBe(false);
    expect(Object.hasOwn(ambiguous({}, baseSpec()), "error")).toBe(false);
  });

  test("a bootstrapping envelope answers with retry_after_ms instead of blocking", () => {
    const env = bootstrapping({ ...baseSpec(), retry_after_ms: 1500, hint: "indexing" });
    expect(env.error?.code).toBe("bootstrapping");
    expect(env.error?.retry_after_ms).toBe(1500);
    expect(env.error?.hint).toBe("indexing");
    expect(env.payload).toBeNull();
  });
});

describe("budget: ceiling, downgrade, minted continuation", () => {
  /** Deterministic generator: a failing run is reproducible from its seed. */
  function mulberry32(seed: number): () => number {
    let state = seed >>> 0;
    return () => {
      state = (state + 0x6d2b79f5) >>> 0;
      let t = state;
      t = Math.imul(t ^ (t >>> 15), t | 1);
      t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
      return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
  }

  interface Item {
    id: number;
    qn: string;
    kind: string;
    file: string;
    line: number;
  }

  function items(count: number, dir: string): Item[] {
    return Array.from({ length: count }, (_, i) => ({
      id: i + 1,
      qn: `${dir}/mod${i}.ts::fn_${i}`,
      kind: "function",
      file: `${dir}/mod${i}.ts`,
      line: i * 10 + 3,
    }));
  }

  /** One tool response: a target plus three lists, which is the shape every handler produces. */
  function payload(calls: Item[], callees: Item[], notes: string[]): Record<string, unknown> {
    return { target: { qn: "src/a.ts::A.run" }, calls, callees, notes };
  }

  test("200 generated payloads stay within budget + 10%, totals preserved, original untouched", () => {
    const rand = mulberry32(0xc0de);
    const violations: string[] = [];
    let truncatedRuns = 0;
    let fittingRuns = 0;

    for (let run = 0; run < 200; run += 1) {
      const calls = items(3 + Math.floor(rand() * 70), "app");
      const callees = items(1 + Math.floor(rand() * 40), "lib");
      const notes = Array.from({ length: Math.floor(rand() * 6) }, (_, i) => `note ${run}-${i}`);
      const spec: EnvelopeSpec = {
        ...baseSpec(),
        totals: {
          total_relation: "eq",
          calls_total: calls.length,
          callees_total: callees.length,
          notes_total: notes.length,
        },
      };
      const env = ok(payload(calls, callees, notes), spec);
      const originalBytes = bytes(env);
      // 400 tokens (1.6 KB) is the smallest budget a full nine-field envelope can honour: below it
      // the fixed skeleton is most of the budget and `emit` has no cut that fits. The boundary is
      // measured in the next test instead of being folded into this one.
      const budgetTokens = 400 + Math.floor(rand() * 2000);
      const budgetBytes = budgetTokens * BYTES_PER_TOKEN;
      const minted: number[] = [];

      const out = emit(env, budgetTokens, {
        mint: (position) => {
          minted.push(position);
          return mint(position);
        },
      });

      const where = `run ${run} (payload ${originalBytes} B, budget ${budgetBytes} B)`;
      const outBytes = bytes(out);

      if (outBytes > budgetBytes * 1.1) {
        violations.push(`${where}: emitted ${outBytes} B, over budget + 10%`);
      }
      if (out.totals.calls_total !== calls.length) {
        violations.push(`${where}: calls_total was not preserved`);
      }
      if (out.totals.callees_total !== callees.length) {
        violations.push(`${where}: callees_total was not preserved`);
      }
      if (out.totals.notes_total !== notes.length) {
        violations.push(`${where}: notes_total was not preserved`);
      }
      if (env.truncated !== false || env.next !== null) {
        violations.push(`${where}: the caller's envelope was mutated`);
      }

      if (originalBytes > budgetBytes) {
        truncatedRuns += 1;
        if (out.truncated !== true) {
          violations.push(`${where}: payload was cut but truncated is not true`);
        }
        if (out.truncation_reason !== "budget") {
          violations.push(`${where}: truncation_reason ${String(out.truncation_reason)}`);
        }
        if (minted.length !== 1) {
          violations.push(`${where}: mint called ${minted.length} times`);
        }
        if (minted[0] !== undefined && out.next !== mint(minted[0])) {
          violations.push(`${where}: next is not the minted continuation`);
        }
        if (out.totals.total_relation !== "gte") {
          violations.push(`${where}: a truncated payload still claims eq`);
        }
      } else {
        fittingRuns += 1;
        if (out.truncated !== false || out.next !== null || minted.length !== 0) {
          violations.push(`${where}: an uncut payload was reported as truncated`);
        }
        if (out.totals.total_relation !== "eq") {
          violations.push(`${where}: an uncut, clean payload did not claim eq`);
        }
      }
    }

    expect(violations).toEqual([]);
    // Both halves were exercised, so the property is not vacuous.
    expect(truncatedRuns).toBeGreaterThan(50);
    expect(fittingRuns).toBeGreaterThan(10);
  });

  test("a truncated payload downgrades eq to gte and carries the minted next", () => {
    const calls = items(120, "app");
    const env = ok(payload(calls, items(60, "lib"), ["one"]), {
      ...baseSpec(),
      totals: { total_relation: "eq", calls_total: 120, callees_total: 60, notes_total: 1 },
    });
    expect(env.totals.total_relation).toBe("eq");
    expect(env.truncated).toBe(false);
    expect(env.next).toBeNull();

    const minted: number[] = [];
    const out = emit(env, 400, {
      mint: (position) => {
        minted.push(position);
        return mint(position);
      },
    });

    expect(out.truncated).toBe(true);
    expect(out.truncation_reason).toBe("budget");
    expect(out.totals.total_relation).toBe("gte");
    expect(out.totals.calls_total).toBe(120);
    expect(out.totals.callees_total).toBe(60);
    expect(minted).toHaveLength(1);
    expect(minted[0]).toBeGreaterThan(0);
    expect(out.next).toBe(mint(minted[0] as number));
    expect(bytes(out)).toBeLessThanOrEqual(400 * BYTES_PER_TOKEN * 1.1);
    // The downgrade and the cut are properties of the returned envelope only.
    expect(env.totals.total_relation).toBe("eq");
    expect(env.truncated).toBe(false);
  });

  test("the ceiling is budget + 10%, or the smallest envelope the payload can be cut to", () => {
    const env = ok(payload(items(80, "app"), items(40, "lib"), ["n"]), {
      ...baseSpec(),
      totals: { total_relation: "eq", calls_total: 80, callees_total: 40, notes_total: 1 },
    });
    // Draining every list is the smallest emittable form of this payload: the skeleton, the
    // `next` token and the preserved totals, none of which the budget may remove.
    const drained = emit(env, 1, { mint: (position) => mint(position) });
    const drainedBytes = bytes(drained);
    expect(drained.truncated).toBe(true);

    const violations: string[] = [];
    for (let budgetTokens = 1; budgetTokens <= 2400; budgetTokens += 1) {
      const budgetBytes = budgetTokens * BYTES_PER_TOKEN;
      const out = emit(env, budgetTokens, { mint: (position) => mint(position) });
      const ceiling = Math.max(budgetBytes * 1.1, drainedBytes + MINTED_TOKEN_BYTES);
      if (bytes(out) > ceiling) {
        violations.push(`${budgetTokens} tokens: ${bytes(out)} B over ceiling ${ceiling} B`);
      }
    }
    expect(violations).toEqual([]);

    // Why the second term is needed: at a budget this small the drained envelope alone is already
    // over the 10% ceiling, so no cut can satisfy it — the budget is honored up to the size the
    // envelope cannot go below, not below it.
    expect(drainedBytes).toBeGreaterThan(100 * BYTES_PER_TOKEN * 1.1);
  });

  test("a payload with no list to shrink is returned unchanged, never mislabelled as truncated", () => {
    const env = ok({ target: { qn: "src/a.ts::A.run" }, note: "x".repeat(4000) }, baseSpec());
    const out = emit(env, 100, { mint: (position) => mint(position) });

    expect(out.truncated).toBe(false);
    expect(out.truncation_reason).toBeNull();
    expect(out.next).toBeNull();
    expect(bytes(out)).toBe(bytes(env));
    // This is the documented limit of the ceiling: there is nothing to drop, so the payload is
    // over budget rather than silently claiming a cut that never happened.
    expect(bytes(out)).toBeGreaterThan(100 * BYTES_PER_TOKEN);
  });
});

describe("cursor: refusals", () => {
  function refusalOf(result: CursorResult): { code: CursorRefusalCode; hint: string } {
    if (result.ok) {
      throw new Error(`expected a refusal, got position ${result.position}`);
    }
    return { code: result.code, hint: result.hint };
  }

  test("a token minted for another tool is refused as cursor_mismatch", () => {
    const token = cursors.sign({ ...CURSOR_CALL, position: 20 });

    const otherTool = refusalOf(cursors.verify(token, { ...CURSOR_CALL, tool: "callers" }));
    expect(otherTool.code).toBe("cursor_mismatch");
    expect(otherTool.hint.length).toBeGreaterThan(0);

    // Same tool, different arguments: a different page of a different query, equally refused.
    const otherArgs = refusalOf(
      cursors.verify(token, { ...CURSOR_CALL, argsHash: `sha256:${"2".repeat(64)}` }),
    );
    expect(otherArgs.code).toBe("cursor_mismatch");

    // The matching call is accepted, so the refusals above are about the call identity.
    expect(cursors.verify(token, CURSOR_CALL)).toEqual({ ok: true, position: 20 });
  });

  test("a token minted in another generation is refused as cursor_expired", () => {
    const token = cursors.sign({ ...CURSOR_CALL, position: 20 });

    const expired = refusalOf(cursors.verify(token, { ...CURSOR_CALL, generation: "g-00000000" }));
    expect(expired.code).toBe("cursor_expired");
    expect(expired.hint.length).toBeGreaterThan(0);

    // Generation is checked before call identity: a stale token that also names another tool
    // still reads "re-run", not "you asked with the wrong arguments".
    const stale = refusalOf(
      cursors.verify(token, { ...CURSOR_CALL, generation: "g-00000000", tool: "callers" }),
    );
    expect(stale.code).toBe("cursor_expired");
  });

  test("a tampered token is refused as cursor_invalid rather than served", () => {
    const token = cursors.sign({ ...CURSOR_CALL, position: 20 });
    const dot = token.indexOf(".");
    const body = token.slice(0, dot);
    const flipped = `${body}${body.endsWith("A") ? "B" : "A"}.${token.slice(dot + 1)}`;

    expect(refusalOf(cursors.verify(flipped, CURSOR_CALL)).code).toBe("cursor_invalid");
    expect(refusalOf(cursors.verify("", CURSOR_CALL)).code).toBe("cursor_invalid");
  });
});

describe("paths: prefix membership", () => {
  test("src-old is not under src, src itself and its children are", () => {
    expect(isUnderPrefix("src-old/a.ts", "src")).toBe(false);
    expect(isUnderPrefix("src/a.ts", "src")).toBe(true);
    expect(isUnderPrefix("src", "src")).toBe(true);
    expect(isUnderPrefix("src/nested/deep/a.ts", "src")).toBe(true);
    expect(isUnderPrefix("srcx/a.ts", "src")).toBe(false);
    expect(isUnderPrefix("other/src/a.ts", "src")).toBe(false);
  });

  test("the bounds exclude the sibling by construction, not by a name check", () => {
    expect(prefixBounds("src")).toEqual({ exact: "src", low: "src/", high: "src0" });
    // `-` (0x2d) sorts below `/` (0x2f): the naive `path >= exact` lower bound is what lets
    // `src-old` in, and `low` is what keeps it out.
    expect("src-old/a.ts" > "src").toBe(true);
    expect("src-old/a.ts" > "src/").toBe(false);
    expect(prefixBounds("./src/")).toEqual({ exact: "src", low: "src/", high: "src0" });
  });
});
