import { describe, expect, test } from "bun:test";
import { BYTES_PER_TOKEN, emit, MAX_BUDGET_TOKENS } from "#shared/budget";
import { type Coverage, type EnvelopeSpec, ok, type ReadStatus } from "#shared/envelope";
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

interface Item {
  id: number;
  qn: string;
  kind: string;
  file: string;
  line: number;
}

function baseSpec(): EnvelopeSpec {
  return {
    summary: "showing N of M",
    read_status: { ...FRESH },
    coverage: { ...CLEAN, not_indexed: [] },
  };
}

function bytes(value: unknown): number {
  return Buffer.byteLength(JSON.stringify(value), "utf8");
}

function items(count: number, prefix: string): Item[] {
  return Array.from({ length: count }, (_, i) => ({
    id: i + 1,
    qn: `${prefix}/mod${i}.py::fn_${i}`,
    kind: "function",
    file: `${prefix}/mod${i}.py`,
    line: 10 * i + 3,
  }));
}

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

function isPrefixOf<T>(candidate: readonly T[], original: readonly T[]): boolean {
  if (candidate.length > original.length) {
    return false;
  }
  return candidate.every((item, index) => JSON.stringify(item) === JSON.stringify(original[index]));
}

describe("emit: budget property", () => {
  test("200 random payloads stay inside budget + 10% with totals preserved", () => {
    const rand = mulberry32(0x5eed);
    const violations: string[] = [];
    let truncatedRuns = 0;
    let fittingRuns = 0;

    for (let run = 0; run < 200; run += 1) {
      const calls = items(5 + Math.floor(rand() * 60), "app");
      const callees = items(5 + Math.floor(rand() * 30), "lib");
      const spec = baseSpec();
      spec.totals = {
        callers_total: calls.length,
        callees_total: callees.length,
        total_relation: "eq",
      };
      const env = ok({ calls: [...calls], callees: [...callees] }, spec);
      const originalBytes = bytes(env);
      const budgetTokens = 400 + Math.floor(rand() * 1600);
      const budgetBytes = budgetTokens * BYTES_PER_TOKEN;
      const minted: number[] = [];
      const out = emit(env, budgetTokens, {
        mint: (position) => {
          minted.push(position);
          return `cur_${position}`;
        },
      });

      const where = `run ${run} (original ${originalBytes} B, budget ${budgetBytes} B)`;
      const outBytes = bytes(out);
      const shouldTruncate = originalBytes > budgetBytes;

      if (outBytes > budgetBytes * 1.1) {
        violations.push(`${where}: emitted ${outBytes} B, over budget + 10%`);
      }
      if (
        out.totals.callers_total !== calls.length ||
        out.totals.callees_total !== callees.length
      ) {
        violations.push(`${where}: a *_total was not preserved (${JSON.stringify(out.totals)})`);
      }
      if (out.totals.total_relation !== "eq" && out.totals.total_relation !== "gte") {
        violations.push(
          `${where}: total_relation ${String(out.totals.total_relation)} is not part of the contract`,
        );
      }
      if (!isPrefixOf(out.payload.calls, calls) || !isPrefixOf(out.payload.callees, callees)) {
        violations.push(`${where}: truncation reordered or rewrote the surviving items`);
      }
      if (
        calls.length !== env.payload.calls.length ||
        callees.length !== env.payload.callees.length
      ) {
        violations.push(`${where}: the caller's payload was mutated`);
      }

      if (shouldTruncate) {
        truncatedRuns += 1;
        if (out.truncated !== true) {
          violations.push(`${where}: truncated is false but the payload was cut`);
        }
        if (minted.length !== 1) {
          violations.push(`${where}: mint called ${minted.length} times`);
        }
        if (out.next !== `cur_${minted[0]}`) {
          violations.push(`${where}: next is not the minted token`);
        }
        if (out.truncation_reason !== "budget") {
          violations.push(
            `${where}: truncation_reason ${String(out.truncation_reason)} is not budget`,
          );
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
          violations.push(`${where}: an uncut, clean payload lost its eq relation`);
        }
      }
    }

    expect({ violations: violations.length, detail: violations }).toEqual({
      violations: 0,
      detail: [],
    });
    expect(truncatedRuns).toBeGreaterThan(0);
    expect(fittingRuns).toBeGreaterThan(0);
    expect(truncatedRuns + fittingRuns).toBe(200);
  });
});

describe("emit: the gte downgrade and the mint callback", () => {
  test("a truncated clean payload is downgraded from eq to gte and carries next from mint", () => {
    const spec = baseSpec();
    spec.totals = { callers_total: 40, callees_total: 4, total_relation: "eq" };
    const env = ok({ calls: items(40, "app"), callees: items(4, "lib") }, spec);
    expect(env.totals.total_relation).toBe("eq");

    const minted: number[] = [];
    const out = emit(env, 900, {
      mint: (position) => {
        minted.push(position);
        return `cur_${position}`;
      },
    });

    expect(out.truncated).toBe(true);
    expect(out.truncation_reason).toBe("budget");
    expect(out.totals.total_relation).toBe("gte");
    expect(out.totals.callers_total).toBe(40);
    expect(out.totals.callees_total).toBe(4);
    expect(minted).toHaveLength(1);
    expect(out.next).toBe(`cur_${minted[0]}`);
    expect(minted[0]).toBeGreaterThan(0);
    expect(bytes(out)).toBeLessThanOrEqual(900 * BYTES_PER_TOKEN * 1.1);
  });

  test("mint is called exactly once and its position equals the number of dropped items", () => {
    const env = ok({ calls: items(50, "app"), callees: items(3, "lib") }, baseSpec());
    const minted: number[] = [];
    const out = emit(env, 700, {
      mint: (position) => {
        minted.push(position);
        return `p${position}`;
      },
    });

    const kept = out.payload.calls.length + out.payload.callees.length;
    const original = 50 + 3;
    expect(minted).toHaveLength(1);
    expect(minted[0]).toBe(original - kept);
    expect(out.next).toBe(`p${minted[0]}`);
  });

  test("the largest list is cut first and the smaller list survives intact", () => {
    const env = ok({ calls: items(60, "app"), callees: items(3, "lib") }, baseSpec());
    const out = emit(env, 1000, { mint: (position) => `cur_${position}` });

    expect(out.payload.callees).toHaveLength(3);
    expect(out.payload.calls.length).toBeLessThan(60);
    expect(out.payload.calls.length).toBeGreaterThan(0);
    expect(isPrefixOf(out.payload.calls, env.payload.calls)).toBe(true);
  });

  test("an envelope that fits is returned untouched and without a mint call", () => {
    const env = ok({ calls: items(2, "app") }, baseSpec());
    let calls = 0;
    const out = emit(env, MAX_BUDGET_TOKENS, {
      mint: () => {
        calls += 1;
        return "cur_never";
      },
    });

    expect(out).toBe(env);
    expect(calls).toBe(0);
    expect(out.truncated).toBe(false);
    expect(out.next).toBeNull();
    expect(out.totals.total_relation).toBe("eq");
  });

  test("the caller's payload arrays are never mutated", () => {
    const calls = items(30, "app");
    const env = ok({ calls }, baseSpec());
    emit(env, 400, { mint: (position) => `cur_${position}` });
    expect(calls).toHaveLength(30);
    expect(env.payload.calls).toHaveLength(30);
  });

  test("a caller-flagged truncation may not keep eq, even when the payload fits", () => {
    const spec = baseSpec();
    spec.totals = { callers_total: 2, total_relation: "eq" };
    spec.truncated = true;
    spec.truncation_reason = "depth";
    spec.next = "cur_depth";
    const env = ok({ calls: items(2, "app") }, spec);

    const out = emit(env, MAX_BUDGET_TOKENS, { mint: () => "cur_never" });
    expect(out.totals.total_relation).toBe("gte");
    expect(out.truncation_reason).toBe("depth");
    expect(out.next).toBe("cur_depth");
    expect(out.totals.callers_total).toBe(2);
  });

  test("nested item lists are reachable by the budget", () => {
    const env = ok(
      {
        groups: [{ name: "calls", derived: false, items: items(40, "app") }],
        callees: items(2, "lib"),
      },
      baseSpec(),
    );
    const out = emit(env, 700, { mint: (position) => `cur_${position}` });
    expect(out.truncated).toBe(true);
    expect(out.totals.total_relation).toBe("gte");
    expect(bytes(out)).toBeLessThanOrEqual(700 * BYTES_PER_TOKEN * 1.1);
  });

  test("a payload with no lists to shrink is returned uncut rather than mislabelled", () => {
    const env = ok({ note: "the whole answer is one sentence" }, baseSpec());
    let calls = 0;
    const out = emit(env, 10, {
      mint: () => {
        calls += 1;
        return "cur_none";
      },
    });

    expect(out.truncated).toBe(false);
    expect(out.next).toBeNull();
    expect(calls).toBe(0);
  });
});

describe("emit: the 32 000 token ceiling", () => {
  test("a budget above the ceiling is refused with budget_too_large", () => {
    const env = ok({ calls: items(5, "app") }, baseSpec());

    for (const budget of [40_000, MAX_BUDGET_TOKENS + 1]) {
      let thrown: unknown;
      try {
        emit(env, budget, { mint: () => "cur" });
      } catch (error) {
        thrown = error;
      }
      expect(thrown).toBeInstanceOf(AppError);
      const appError = thrown as AppError;
      expect(appError.code).toBe("budget_too_large");
      expect(appError.message).toContain(String(budget));
      expect(appError.hint).toBeDefined();
    }
  });

  test("the ceiling itself is accepted", () => {
    const env = ok({ calls: items(5, "app") }, baseSpec());
    const out = emit(env, MAX_BUDGET_TOKENS, { mint: () => "cur" });
    expect(out.status).toBe("ok");
    expect(MAX_BUDGET_TOKENS * BYTES_PER_TOKEN).toBe(128_000);
  });
});

/**
 * The envelope-level list. `coverage.not_indexed` is part of the serialised answer, so a payload
 * whose own lists are small can still breach the ceiling on it alone (`summary` on the reference
 * corpus did: 6 103 B against a 5 280 B ceiling, with 126 B of payload). These tests pin the two
 * halves of the fix: the list is trimmable, and trimming it keeps the coverage claim honest.
 *
 * `budgetByteAllowance(tokens)` is `tokens * BYTES_PER_TOKEN` for a budget inside the ceiling, so
 * every ceiling here is written that way rather than reaching into the module's private helper.
 */
describe("emit: trimming the envelope-level coverage list", () => {
  const BUDGET_TOKENS = 800;
  const ALLOWANCE = BUDGET_TOKENS * BYTES_PER_TOKEN;

  interface Skipped {
    path: string;
    reason: string;
  }

  /** Skipped paths in the reader's own order (path ascending), which is what a trim cuts from the end. */
  function skipped(count: number): Skipped[] {
    return Array.from({ length: count }, (_, i) => ({
      path: `code_review_graph/module_${String(i).padStart(3, "0")}.xslt`,
      reason: "unsupported_ext",
    }));
  }

  /** Coverage over a scope that skipped `notIndexed.length` paths: incomplete, and it says so. */
  function skippedSpec(notIndexed: readonly Skipped[]): EnvelopeSpec {
    return {
      summary: `${notIndexed.length} paths were looked at and skipped`,
      read_status: { ...FRESH },
      coverage: {
        complete: false,
        indexed: 400,
        skipped: notIndexed.length,
        failed: 0,
        unresolved_edges: 0,
        not_indexed: [...notIndexed],
      },
    };
  }

  function mintSpy(): { mint: (position: number) => string; positions: number[] } {
    const positions: number[] = [];
    return {
      positions,
      mint: (position) => {
        positions.push(position);
        return `cur_${position}`;
      },
    };
  }

  test("a large coverage.not_indexed is trimmed until the envelope fits the allowance", () => {
    const entries = skipped(80);
    const env = ok(
      { base: "HEAD", risk: { score: 0, band: "low", drivers: ["changed_files", "untested"] } },
      skippedSpec(entries),
    );
    // The shape the defect was reproduced on: the payload is 126 B and the envelope is 6 KB.
    expect(bytes(env)).toBeGreaterThan(ALLOWANCE);

    const spy = mintSpy();
    const out = emit(env, BUDGET_TOKENS, { mint: spy.mint });

    expect(bytes(out)).toBeLessThanOrEqual(ALLOWANCE);
    expect(out.truncated).toBe(true);
    expect(out.truncation_reason).toBe("budget");
    expect(out.totals.total_relation).toBe("gte");

    // Coverage stays honest: the count is exact, `complete` stays false, and what is left is the
    // head of the reader's path order rather than an arbitrary sample.
    expect(out.coverage.skipped).toBe(80);
    expect(out.coverage.complete).toBe(false);
    expect(out.coverage.not_indexed.length).toBeGreaterThan(0);
    expect(out.coverage.not_indexed.length).toBeLessThan(80);
    expect(isPrefixOf(out.coverage.not_indexed, entries)).toBe(true);

    // The payload's own list is drained first, then the coverage list pays for the rest, and the
    // count `emit` reports is everything that left the envelope — rows and paths alike.
    expect(out.payload.risk.drivers).toHaveLength(0);
    const droppedRows = env.payload.risk.drivers.length - out.payload.risk.drivers.length;
    const droppedPaths = entries.length - out.coverage.not_indexed.length;
    expect(spy.positions).toHaveLength(1);
    expect(spy.positions[0]).toBe(droppedRows + droppedPaths);
    expect(out.next).toBe(`cur_${spy.positions[0]}`);
  });

  test("a path the payload answers about survives the trim; the unpinned tail does not", () => {
    // Sorts last, so a trim that only knew about paths would drop it first.
    const touched = "code_review_graph/zzz_touched.xslt";
    const entries = [...skipped(80), { path: touched, reason: "grammar_unavailable" }];

    const answering = ok(
      { paths: [{ path: touched, indexed: false, reason: "grammar_unavailable" }] },
      skippedSpec(entries),
    );
    const blind = ok({ paths: [] as { path: string }[] }, skippedSpec(entries));

    const kept = emit(answering, BUDGET_TOKENS, { mint: (position) => `cur_${position}` });
    const cut = emit(blind, BUDGET_TOKENS, { mint: (position) => `cur_${position}` });

    const keptPaths = kept.coverage.not_indexed.map((entry) => entry.path);
    const cutPaths = cut.coverage.not_indexed.map((entry) => entry.path);

    // The control proves the pin changed the outcome rather than the trim being lenient.
    expect(keptPaths).toContain(touched);
    expect(cutPaths).not.toContain(touched);
    expect(keptPaths.length).toBeLessThan(entries.length);
    expect(kept.coverage.skipped).toBe(entries.length);
    expect(bytes(kept)).toBeLessThanOrEqual(ALLOWANCE);
  });

  test("the caller's coverage.not_indexed array is never mutated by a trim", () => {
    const entries = skipped(80);
    const env = ok({ base: "HEAD" }, skippedSpec(entries));
    const before = env.coverage.not_indexed.map((entry) => entry.path);

    const out = emit(env, BUDGET_TOKENS, { mint: (position) => `cur_${position}` });

    expect(out.coverage.not_indexed.length).toBeLessThan(before.length);
    // Neither the envelope the caller passed in nor the fixture it was built from lost an entry.
    expect(env.coverage.not_indexed.map((entry) => entry.path)).toEqual(before);
    expect(entries.map((entry) => entry.path)).toEqual(before);
    expect(env.truncated).toBe(false);
    expect(env.next).toBeNull();
    // The trimmed list is the draft's own array: the copy is what makes the mutation safe.
    expect(out.coverage.not_indexed).not.toBe(env.coverage.not_indexed);
  });

  test("a list a cut took with its parent is not counted as trimmed", () => {
    // `groups` holds two groups of 40 items; `spill` is a further 60 items beside it. The budget is
    // small enough that draining `groups` is not enough, so the pass has to reach `spill` — and the
    // 80 items that left with the groups are not lists it may cut a second time.
    const env = ok(
      {
        groups: [
          { name: "calls", derived: false, items: items(40, "app") },
          { name: "calls", derived: false, items: items(40, "lib") },
        ],
        spill: items(60, "lib"),
      },
      baseSpec(),
    );

    const spy = mintSpy();
    const out = emit(env, 500, { mint: spy.mint });

    expect(bytes(out)).toBeLessThanOrEqual(500 * BYTES_PER_TOKEN);
    expect(out.payload.groups).toHaveLength(0);
    expect(out.payload.spill.length).toBeGreaterThan(0);
    expect(out.payload.spill.length).toBeLessThan(60);

    // Two groups plus the spill rows that went: counting the detached lists would report ~80 more
    // drops than the envelope lost, and a client resuming from that count would skip rows.
    expect(spy.positions).toHaveLength(1);
    expect(spy.positions[0]).toBe(2 + (60 - out.payload.spill.length));
  });

  test("every budget from the envelope's floor upwards is honoured, with one mint and kept totals", () => {
    const calls = items(40, "app");
    const entries = skipped(30);
    const spec = skippedSpec(entries);
    spec.totals = {
      total_relation: "gte",
      calls_total: calls.length,
      skipped_total: entries.length,
    };

    // The floor: every list drained, so what is left is the skeleton, the required fields and the
    // token. Rounding it up to whole tokens is the smallest budget the envelope can honour.
    const floor = emit(ok({ calls: [] as Item[] }, skippedSpec(entries)), BUDGET_TOKENS, {
      mint: (position) => `cur_${position}`,
    });
    const smallestHonoured = Math.ceil(bytes(floor) / BYTES_PER_TOKEN);

    const violations: string[] = [];
    let cutRuns = 0;
    let wholeRuns = 0;
    for (const budgetTokens of [
      smallestHonoured,
      smallestHonoured + 1,
      400,
      600,
      900,
      1200,
      2000,
    ]) {
      const env = ok({ calls: [...calls] }, spec);
      const spy = mintSpy();
      const out = emit(env, budgetTokens, { mint: spy.mint });
      const where = `${budgetTokens} tokens (${budgetTokens * BYTES_PER_TOKEN} B)`;
      const allowance = budgetTokens * BYTES_PER_TOKEN;

      if (bytes(out) > allowance) {
        violations.push(`${where}: emitted ${bytes(out)} B`);
      }
      if (out.totals.calls_total !== calls.length || out.totals.skipped_total !== entries.length) {
        violations.push(`${where}: a *_total was not preserved (${JSON.stringify(out.totals)})`);
      }
      if (isPrefixOf(out.payload.calls, calls) === false) {
        violations.push(`${where}: the surviving rows are not the ranked head`);
      }
      if (isPrefixOf(out.coverage.not_indexed, entries) === false) {
        violations.push(`${where}: the surviving paths are not the path-order head`);
      }

      if (out.truncated) {
        cutRuns += 1;
        if (out.truncation_reason !== "budget") {
          violations.push(`${where}: truncation_reason ${String(out.truncation_reason)}`);
        }
        if (out.totals.total_relation !== "gte") {
          violations.push(`${where}: a cut envelope still claims eq`);
        }
        if (spy.positions.length !== 1) {
          violations.push(`${where}: mint called ${spy.positions.length} times`);
        }
        const droppedRows = calls.length - out.payload.calls.length;
        const droppedPaths = entries.length - out.coverage.not_indexed.length;
        if (spy.positions[0] !== droppedRows + droppedPaths) {
          violations.push(
            `${where}: minted ${String(spy.positions[0])} for ${droppedRows + droppedPaths} dropped items`,
          );
        }
        if (out.next !== `cur_${String(spy.positions[0])}`) {
          violations.push(`${where}: next is not the minted token`);
        }
      } else {
        wholeRuns += 1;
        if (spy.positions.length !== 0) {
          violations.push(`${where}: mint called without a cut`);
        }
        if (out.next !== null) {
          violations.push(`${where}: next present without a cut`);
        }
      }
    }

    expect({ violations: violations.length, detail: violations }).toEqual({
      violations: 0,
      detail: [],
    });
    // Both halves ran, so the sweep is not vacuous.
    expect(cutRuns).toBeGreaterThan(0);
    expect(wholeRuns).toBeGreaterThan(0);
  });

  /**
   * The floor is the one budget `emit` cannot honour, and the rule it follows there is the same one
   * it follows for a payload with nothing to cut: the envelope comes back **unchanged**. A drained
   * copy labelled `truncated: true` would claim a cut the caller can act on while still being over
   * its allowance, and the position it minted would be the one the call arrived with — the head of a
   * page that cannot advance, which is how `expand` at `budget: 1` used to hand a client the same
   * page forever.
   *
   * The regime is bounded: the floor here is well under the smallest declared default budget
   * (`status`: 800 tokens = 3 200 B), so no call at a declared default can reach it.
   */
  test("below the envelope's own floor the envelope is returned untouched and unlabelled", () => {
    const calls = items(40, "app");
    const entries = skipped(20);
    const spec = skippedSpec(entries);
    spec.totals = { total_relation: "gte", calls_total: calls.length };
    const env = ok({ calls: [...calls] }, spec);

    const spy = mintSpy();
    const out = emit(env, 1, { mint: spy.mint });

    // Nothing was cut, so nothing may say it was, and no token may point at a page that would be
    // byte-identical to this one.
    expect(out.truncated).toBe(false);
    expect(out.truncation_reason).toBeNull();
    expect(out.next).toBeNull();
    expect(spy.positions).toHaveLength(0);
    expect(out).toEqual(env);
    expect(bytes(out)).toBe(bytes(env));

    // A caller-flagged truncation survives the floor untouched, so `emit` never erases the caller's
    // own claim (`SearchEnvelope`-style zero-row pages depend on that).
    const flagged = emit({ ...env, truncated: true, truncation_reason: "budget" }, 1, {
      mint: spy.mint,
    });
    expect(flagged.truncated).toBe(true);
    expect(flagged.totals.total_relation).toBe("gte");
    expect(spy.positions).toHaveLength(0);

    // The floor is a floor, not a leak: just under it the same payload is cut and minted.
    const smallestHonoured = Math.ceil(bytes(env) / BYTES_PER_TOKEN);
    const cut = emit(env, Math.max(2, smallestHonoured - 2), { mint: spy.mint });
    expect(bytes(cut)).toBeLessThanOrEqual((smallestHonoured - 2) * BYTES_PER_TOKEN);
    expect(cut.truncated).toBe(true);
    expect(spy.positions).toHaveLength(1);
  });
});
