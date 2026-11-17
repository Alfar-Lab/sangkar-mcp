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
