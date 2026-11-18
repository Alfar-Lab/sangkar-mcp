import { type Envelope, withPayload, withTotals } from "#shared/envelope";
import { AppError } from "#shared/errors";

/** The declared bytes-per-token ratio every budget decision uses (DESIGN.md §7.1). */
export const BYTES_PER_TOKEN = 4;

/** Ceiling above which `emit` refuses with `budget_too_large` (TOOLS-SPEC.md §9). */
export const MAX_BUDGET_TOKENS = 32_000;

/**
 * Characters reserved for the continuation token while truncating. `next` is only minted once the
 * drop count is final, so the measurement has to charge for a token it has not seen yet — and a
 * short charge is spent by the real token afterwards: a signed cursor for this app measures 211
 * bytes (base64 JSON envelope, a dot, then the HMAC), so a 128-byte reserve left budget-filling
 * replies up to 83 bytes over their own ceiling. 256 covers it with room for a longer cursor.
 */
const NEXT_TOKEN_RESERVE_CHARS = 256;

/** Bytes a drained array serialises to (`[]`): the floor a list can be cut to, never below. */
const EMPTY_LIST_BYTES = 2;

/**
 * How many target-driven passes one `emit` may run. Real payloads settle in one, because each pass
 * computes the exact byte cost of what it drops; the cap only bounds a pathological payload.
 */
const MAX_TRIM_PASSES = 8;

/** Deep enough for every tool payload (the deepest is an array of groups of items). */
const MAX_PIN_DEPTH = 6;

export interface EmitOptions {
  /**
   * Mints `next` for a truncation that dropped `position` items. Signing is the caller's job:
   * this module knows nothing about cursors, only that a cut must never be silent.
   */
  mint: (position: number) => string;
}

/**
 * One trimmable array. `bytes` is the serialised size measured when the slot was collected; it
 * decides which list is cut first and is never used as accounting (a cut above or below it makes
 * it stale, and every byte decision is recomputed from {@link itemBytes}).
 *
 * `parent`/`parentIndex` record the array that holds this one: a cut that removes the item holding
 * a nested list takes that list with it.
 */
interface ListSlot {
  items: unknown[];
  bytes: number;
  parent: ListSlot | null;
  parentIndex: number;
  itemBytes: number[] | undefined;
}

/** What one cut costs the envelope: how many items left, and the bytes those items were worth. */
interface DropPlan {
  count: number;
  saved: number;
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

/**
 * Collects every array in the payload, remembering which array holds it so a cut above can detach
 * a nested list instead of accounting for bytes that are no longer in the envelope.
 */
function collectListSlots(
  value: unknown,
  out: ListSlot[],
  parent: ListSlot | null,
  parentIndex: number,
): void {
  if (Array.isArray(value)) {
    const slot: ListSlot = {
      items: value,
      bytes: listSize(value),
      parent,
      parentIndex,
      itemBytes: undefined,
    };
    out.push(slot);
    for (let index = 0; index < value.length; index += 1) {
      collectListSlots(value[index], out, slot, index);
    }
    return;
  }
  if (typeof value === "object" && value !== null) {
    for (const item of Object.values(value)) {
      collectListSlots(item, out, parent, parentIndex);
    }
  }
}

/** False once a cut in an enclosing list has removed the item this array lived in. */
function isAttached(slot: ListSlot): boolean {
  let node = slot;
  while (node.parent !== null) {
    if (node.parentIndex >= node.parent.items.length) {
      return false;
    }
    node = node.parent;
  }
  return true;
}

/** Serialised size of each item, computed once per slot that is actually cut. */
function itemSizes(slot: ListSlot): readonly number[] {
  if (slot.itemBytes === undefined) {
    slot.itemBytes = slot.items.map((item) => {
      // An array element that is `undefined` serialises as `null`, not as nothing.
      const text = JSON.stringify(item) as string | undefined;
      return Buffer.byteLength(text === undefined ? "null" : text, "utf8");
    });
  }
  return slot.itemBytes;
}

/** `[a,b]` is two brackets, one comma per gap and the items themselves; `[]` is just the brackets. */
function listBytes(sizes: readonly number[]): number {
  let total = sizes.length + 1;
  for (const size of sizes) {
    total += size;
  }
  return total;
}

/** Largest list first; ties go to the longer one, so the choice is deterministic (§7.1 rule 2). */
function byLargestFirst(a: ListSlot, b: ListSlot): number {
  return b.bytes - a.bytes || b.items.length - a.items.length;
}

/**
 * How many tail items have to go to free `excess` bytes, and what that frees. The sizes come from
 * the list itself, so this is one pass over the items being dropped rather than a re-serialisation
 * of the whole envelope per removed row — the difference between O(total bytes) and
 * O(dropped × envelope bytes) for a dense payload.
 */
function planTailDrop(slot: ListSlot, excess: number): DropPlan {
  const sizes = itemSizes(slot);
  const floor = listBytes(sizes) - EMPTY_LIST_BYTES;
  let saved = 0;
  let count = 0;
  while (count < sizes.length && saved < excess) {
    // A removed item takes the comma that separated it from its predecessor with it.
    saved += (sizes[sizes.length - 1 - count] ?? 0) + 1;
    count += 1;
  }
  return { count, saved: Math.min(saved, floor) };
}

function applyTailDrop(slot: ListSlot, count: number): void {
  slot.items.length -= count;
  // The sizes are stale now, here and in every list enclosing this one.
  for (let node: ListSlot | null = slot; node !== null; node = node.parent) {
    node.itemBytes = undefined;
  }
}

/**
 * Trims `coverage.not_indexed`, the envelope-level list, from the low-path end. An entry whose path
 * the payload already answers about is pinned to the last pass, so a request that touched a skipped
 * path keeps its reason (`coverage.complete` stays false and `coverage.skipped` stays exact, so the
 * count still says how many paths are missing). Pinned entries are only dropped when the clamped
 * ones cannot pay for the cut at all — the budget is the one guarantee that outranks them.
 */
function trimCoverageTail(slot: ListSlot, excess: number, pinned: ReadonlySet<string>): DropPlan {
  const sizes = itemSizes(slot);
  const floor = listBytes(sizes) - EMPTY_LIST_BYTES;
  const cut: boolean[] = new Array<boolean>(sizes.length).fill(false);
  let saved = 0;
  let count = 0;

  for (const pinnedPass of [false, true]) {
    for (let index = sizes.length - 1; index >= 0 && saved < excess; index -= 1) {
      if (cut[index] === true) {
        continue;
      }
      const path = (slot.items[index] as { path?: unknown } | null | undefined)?.path;
      if ((typeof path === "string" && pinned.has(path)) !== pinnedPass) {
        continue;
      }
      cut[index] = true;
      saved += (sizes[index] ?? 0) + 1;
      count += 1;
    }
    if (saved >= excess) {
      break;
    }
  }

  if (count > 0) {
    const kept = slot.items.filter((_, index) => cut[index] !== true);
    slot.items.length = 0;
    for (const entry of kept) {
      slot.items.push(entry);
    }
    slot.itemBytes = undefined;
  }
  return { count, saved: Math.min(saved, floor) };
}

/**
 * Every string the payload carries. A `not_indexed` entry whose path is one of them is the reason
 * for a path the answer itself names, which is what "the request touched it" looks like from here.
 */
function pathsAnsweredBy(payload: unknown): Set<string> {
  const seen = new Set<string>();
  const walk = (value: unknown, depth: number): void => {
    if (depth > MAX_PIN_DEPTH) {
      return;
    }
    if (typeof value === "string") {
      seen.add(value);
      return;
    }
    if (Array.isArray(value)) {
      for (const item of value) {
        walk(item, depth + 1);
      }
      return;
    }
    if (typeof value === "object" && value !== null) {
      for (const item of Object.values(value)) {
        walk(item, depth + 1);
      }
    }
  };
  walk(payload, 0);
  return seen;
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
 * One target-driven pass: free `excess` bytes by cutting ranked payload lists first (the largest
 * one, lowest-ranked tail item first), then the envelope-level coverage explanation.
 *
 * Lists are cut parents-before-children because a parent's serialised size always contains its
 * children's, and a nested list is skipped once the item that held it has been dropped.
 */
function trimOnce(
  payloadSlots: readonly ListSlot[],
  coverage: ListSlot,
  excess: number,
  pinnedPaths: () => ReadonlySet<string>,
): DropPlan {
  let remaining = excess;
  let count = 0;
  let saved = 0;

  for (const slot of [...payloadSlots].sort(byLargestFirst)) {
    if (remaining <= 0) {
      break;
    }
    if (slot.items.length === 0 || !isAttached(slot)) {
      continue;
    }
    const plan = planTailDrop(slot, remaining);
    if (plan.count === 0) {
      continue;
    }
    applyTailDrop(slot, plan.count);
    count += plan.count;
    saved += plan.saved;
    remaining -= plan.saved;
  }

  if (remaining > 0 && coverage.items.length > 0) {
    const plan = trimCoverageTail(coverage, remaining, pinnedPaths());
    count += plan.count;
    saved += plan.saved;
    remaining -= plan.saved;
  }

  return { count, saved };
}

/**
 * The single place a payload is truncated (DESIGN.md §7.1). Serialise, measure at
 * `BYTES_PER_TOKEN` bytes per token, then cut the lowest-ranked items of the largest list until the
 * envelope fits; every `<list>_total` is preserved, `truncated`, `truncation_reason` and `next` are
 * set, and `totals.total_relation` is downgraded from `eq` to `gte`.
 *
 * The candidates are the payload's own lists and the one envelope-level list,
 * `coverage.not_indexed`: a large skip list is part of the answer's size, so it has to be cuttable
 * or the ceiling is unenforceable. Payload lists go first because they are the answer; coverage is
 * the explanation of it. Trimming coverage keeps the arithmetic honest — `coverage.skipped` stays
 * the exact count, `coverage.complete` stays false, and the paths the payload answers about are
 * pinned to the last pass — so a shortened list still reads "15 of 80 shown".
 *
 * `opts.mint` is called exactly once and only when something was actually dropped, with the number
 * of items (payload and coverage) that left the envelope. A payload with no lists to shrink and no
 * coverage to abridge cannot be cut, so it is returned unchanged rather than mislabelled as
 * truncated. An envelope whose own skeleton already exceeds the allowance is the same case from the
 * other side — the budget is below the floor the required fields cost — so it is returned untouched
 * too: no false `truncated`, and no token pointing at a page that cannot advance.
 * Budgets above `MAX_BUDGET_TOKENS` are refused: sizing is not a per-tool choice.
 */
export function emit<T>(env: Envelope<T>, budgetTokens: number, opts: EmitOptions): Envelope<T> {
  const budgetBytes = budgetByteAllowance(budgetTokens);

  if (byteSize(env) <= budgetBytes) {
    return env.truncated ? asLowerBound(env) : env;
  }

  const payload = jsonClone(env.payload);
  const draft = withPayload(env, payload);
  // The coverage list is trimmable too, so it has to be a copy: the caller's envelope keeps it whole.
  const notIndexed = [...env.coverage.not_indexed];
  draft.coverage = { ...env.coverage, not_indexed: notIndexed };
  draft.truncated = true;
  draft.truncation_reason = "budget";
  // Placeholder: it charges the measurement for the token that has not been minted yet.
  draft.next = "x".repeat(NEXT_TOKEN_RESERVE_CHARS);
  draft.totals = { ...draft.totals, total_relation: "gte" };

  const payloadSlots: ListSlot[] = [];
  collectListSlots(payload, payloadSlots, null, 0);
  const coverageSlot: ListSlot = {
    items: notIndexed,
    bytes: listSize(notIndexed),
    parent: null,
    parentIndex: 0,
    itemBytes: undefined,
  };

  // Read from the caller's payload, not from the draft: a payload list is drained before the
  // coverage list is touched, so the answer's own mention of a path can be gone by then — and the
  // pin exists precisely to survive that cut.
  let pinned: Set<string> | null = null;
  const pinnedPaths = (): ReadonlySet<string> => {
    pinned ??= pathsAnsweredBy(env.payload);
    return pinned;
  };

  let dropped = 0;
  let measured = byteSize(draft);
  for (let pass = 0; pass < MAX_TRIM_PASSES && measured > budgetBytes; pass += 1) {
    const plan = trimOnce(payloadSlots, coverageSlot, measured - budgetBytes, pinnedPaths);
    if (plan.count === 0) {
      break;
    }
    dropped += plan.count;
    measured = byteSize(draft);
  }

  if (dropped === 0) {
    return env.truncated ? asLowerBound(env) : env;
  }

  // The floor: every droppable list is gone and the envelope still does not fit, so the budget is
  // below what the required fields alone cost. A drained copy labelled `truncated` would claim a cut
  // the caller can act on while still being over its allowance, and the minted position would be the
  // one the call came in with — the head of a page that cannot advance. The honest answer is the
  // envelope as it arrived: unchanged, unlabelled, and with no token `mint` was never asked for.
  if (measured > budgetBytes) {
    return env.truncated ? asLowerBound(env) : env;
  }

  draft.next = opts.mint(dropped);
  return draft;
}

/**
 * The smallest budget this answer can be delivered at: its serialised bytes at the declared exchange
 * rate, rounded up. `emit` returns an answer untouched when the budget is below this — the floor the
 * required fields cost — and a tool refuses then instead of serving it, because a response many
 * times the caller's allowance is not an answer to its budget.
 */
export function budgetFloorOf(value: unknown): number {
  return Math.ceil(byteSize(value) / BYTES_PER_TOKEN);
}
