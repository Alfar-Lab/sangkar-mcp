import { type Envelope, withPayload, withTotals } from "#shared/envelope";
import { AppError } from "#shared/errors";

/** The declared bytes-per-token ratio every budget decision uses (DESIGN.md §7.1). */
export const BYTES_PER_TOKEN = 4;

/** Ceiling above which `emit` refuses with `budget_too_large` (TOOLS-SPEC.md §9). */
export const MAX_BUDGET_TOKENS = 32_000;

/**
 * Characters reserved for the continuation token while truncating. `next` is only minted once
 * the drop count is final, so the measurement has to charge for a token it has not seen yet.
 */
const NEXT_TOKEN_RESERVE_CHARS = 128;

export interface EmitOptions {
  /**
   * Mints `next` for a truncation that dropped `position` items. Signing is the caller's job:
   * this module knows nothing about cursors, only that a cut must never be silent.
   */
  mint: (position: number) => string;
}

/** A mutable array found in the payload, with its current serialised size. */
interface ListSlot {
  items: unknown[];
  bytes: number;
}

function byteSize(value: unknown): number {
  return Buffer.byteLength(JSON.stringify(value), "utf8");
}

function listSize(items: readonly unknown[]): number {
  return byteSize(items);
}

function jsonClone<T>(value: T): T {
  const text = JSON.stringify(value) as string | undefined;
  if (text === undefined) {
    return value;
  }
  return JSON.parse(text) as T;
}

/** Collects every array in the payload; nested item lists are candidates too. */
function collectListSlots(value: unknown, out: ListSlot[]): void {
  if (Array.isArray(value)) {
    out.push({ items: value, bytes: listSize(value) });
    for (const item of value) {
      collectListSlots(item, out);
    }
    return;
  }
  if (typeof value === "object" && value !== null) {
    for (const item of Object.values(value)) {
      collectListSlots(item, out);
    }
  }
}

/**
 * The largest list that still has an item to give. Tools order their lists best-first, so the
 * last element is the lowest-ranked one and is what gets dropped (DESIGN.md §7.1 rule 2).
 */
function largestNonEmptySlot(slots: readonly ListSlot[]): ListSlot | undefined {
  let best: ListSlot | undefined;
  for (const slot of slots) {
    if (slot.items.length === 0) {
      continue;
    }
    if (
      best === undefined ||
      slot.bytes > best.bytes ||
      (slot.bytes === best.bytes && slot.items.length > best.items.length)
    ) {
      best = slot;
    }
  }
  return best;
}

function budgetByteAllowance(budgetTokens: number): number {
  if (!Number.isFinite(budgetTokens)) {
    throw new AppError("internal", `budget must be a finite number, got ${String(budgetTokens)}`);
  }
  if (budgetTokens > MAX_BUDGET_TOKENS) {
    throw new AppError(
      "budget_too_large",
      `budget ${budgetTokens} exceeds the ${MAX_BUDGET_TOKENS} token ceiling`,
      {
        hint: `request at most ${MAX_BUDGET_TOKENS} tokens`,
      },
    );
  }
  return Math.max(0, Math.floor(budgetTokens)) * BYTES_PER_TOKEN;
}

/** A payload flagged as truncated may never claim an exact total, even if it fits. */
function asLowerBound<T>(env: Envelope<T>): Envelope<T> {
  if (env.totals.total_relation === "gte") {
    return env;
  }
  return withTotals(env, { ...env.totals, total_relation: "gte" });
}

/**
 * The single place a payload is truncated (DESIGN.md §7.1). Serialise, measure at
 * `BYTES_PER_TOKEN` bytes per token, then drop the lowest-ranked items of the largest list until
 * the envelope fits; every `<list>_total` is preserved, `truncated`, `truncation_reason` and
 * `next` are set, and `totals.total_relation` is downgraded from `eq` to `gte`.
 *
 * `opts.mint` is called exactly once and only when something was actually dropped. A payload
 * with no lists to shrink cannot be cut, so it is returned unchanged rather than mislabelled as
 * truncated. Budgets above `MAX_BUDGET_TOKENS` are refused: sizing is not a per-tool choice.
 */
export function emit<T>(env: Envelope<T>, budgetTokens: number, opts: EmitOptions): Envelope<T> {
  const budgetBytes = budgetByteAllowance(budgetTokens);

  if (byteSize(env) <= budgetBytes) {
    return env.truncated ? asLowerBound(env) : env;
  }

  const payload = jsonClone(env.payload);
  const slots: ListSlot[] = [];
  collectListSlots(payload, slots);

  const draft = withPayload(env, payload);
  draft.truncated = true;
  draft.truncation_reason = "budget";
  // Placeholder: it charges the measurement for the token that has not been minted yet.
  draft.next = "x".repeat(NEXT_TOKEN_RESERVE_CHARS);
  draft.totals = { ...draft.totals, total_relation: "gte" };

  let dropped = 0;
  while (byteSize(draft) > budgetBytes) {
    const slot = largestNonEmptySlot(slots);
    if (slot === undefined) {
      break;
    }
    slot.items.pop();
    slot.bytes = listSize(slot.items);
    dropped += 1;
  }

  if (dropped === 0) {
    return env.truncated ? asLowerBound(env) : env;
  }

  draft.next = opts.mint(dropped);
  return draft;
}
