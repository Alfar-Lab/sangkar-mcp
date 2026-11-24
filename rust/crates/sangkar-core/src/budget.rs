//! [`emit`]: the single place a payload is truncated and the only minter of `next`.
//!
//! Ported from `src/shared/budget.ts`. Serialise, measure at [`BYTES_PER_TOKEN`] bytes per token,
//! then cut the lowest-ranked items of the largest list until the envelope fits. Every
//! `<list>_total` is preserved, `truncated` / `truncation_reason` / `next` are set and
//! `totals.total_relation` is downgraded from `eq` to `gte`.
//!
//! The candidates are the payload's own arrays and the one envelope-level list,
//! `coverage.not_indexed`: a large skip list is part of the answer's size, so it has to be cuttable
//! or the ceiling is unenforceable. Payload lists go first because they are the answer; coverage is
//! the explanation of it. Trimming coverage keeps the arithmetic honest — `coverage.skipped` stays
//! the exact count, `coverage.complete` stays false, and the paths the payload answers about are
//! pinned to the last pass — so a shortened list still reads "15 of 80 shown".
//!
//! Measurement is one compact-JSON serialisation of the whole envelope through `serde_json::Value`,
//! which is also the value that is returned: what is measured and what is emitted cannot drift.
//! Payload scalars are JSON-native (string, bool, integer, `f64`); an `f32` field would be widened
//! to `f64` by that round trip.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::envelope::{Envelope, TotalRelation, TruncationReason, with_totals};
use crate::errors::{AppError, ErrorCode};

/// The declared bytes-per-token ratio every budget decision uses.
pub const BYTES_PER_TOKEN: u64 = 4;

/// Ceiling above which [`emit`] refuses with `budget_too_large`: sizing is not a per-tool choice.
pub const MAX_BUDGET_TOKENS: u64 = 32_000;

/// Characters reserved for the continuation token while truncating.
///
/// `next` is only minted once the drop count is final, so the measurement has to charge for a token
/// it has not seen yet — and a short charge is spent by the real token afterwards: a signed cursor
/// for this app measures 211 bytes unbound and 298 with this repository's canonical path stamped
/// into it, so a 128-byte reserve left budget-filling replies up to 83 bytes over their own ceiling,
/// and a 256-byte reserve broke at 298. 384 is the ceiling `tests/envelope_budget.rs` pins: a longer
/// canonical path than the reserve covers fails there rather than in a client.
const NEXT_TOKEN_RESERVE_CHARS: usize = 384;

/// Bytes a drained array serialises to (`[]`): the floor a list can be cut to, never below.
const EMPTY_LIST_BYTES: u64 = 2;

/// How many target-driven passes one `emit` may run. Real payloads settle in one, because each pass
/// computes the exact byte cost of what it drops; the cap only bounds a pathological payload.
const MAX_TRIM_PASSES: usize = 8;

/// Deep enough for every tool payload (the deepest is an array of groups of items).
const MAX_PIN_DEPTH: usize = 6;

/// One step of a path from a root value down to a trimmable array.
#[derive(Clone, Debug)]
enum Step {
    Key(String),
    Index(usize),
}

/// One trimmable array, addressed by path from the payload root.
///
/// `bytes` is the serialised size measured when the slot was collected; it decides which list is
/// cut first and is never used as accounting (a cut above or below it makes it stale, and every byte
/// decision is recomputed from the items' own sizes).
#[derive(Clone, Debug)]
struct Slot {
    path: Vec<Step>,
    bytes: u64,
}

/// What one cut costs the envelope: how many items left, and the bytes those items were worth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DropPlan {
    count: usize,
    saved: u64,
}

/// Compact-JSON length of any serialisable value.
fn json_len<T: Serialize + ?Sized>(value: &T) -> u64 {
    serde_json::to_value(value).map_or(0, |value| value.to_string().len() as u64)
}

/// `[a,b]` is two brackets, one comma per gap and the items themselves; `[]` is just the brackets.
fn list_bytes(sizes: &[u64]) -> u64 {
    (sizes.len() as u64) + 1 + sizes.iter().sum::<u64>()
}

/// Serialised size of each item, recomputed from the list that is about to be cut.
fn item_sizes(items: &[Value]) -> Vec<u64> {
    items.iter().map(json_len).collect()
}

/// Resolves `path` inside `root`; `None` once a step left the value, which is what a cut in an
/// enclosing list looks like from here.
fn resolve<'a>(root: &'a Value, path: &[Step]) -> Option<&'a Value> {
    let mut node = root;
    for step in path {
        node = match (step, node) {
            (Step::Key(key), Value::Object(map)) => map.get(key)?,
            (Step::Index(index), Value::Array(items)) => items.get(*index)?,
            _ => return None,
        };
    }
    Some(node)
}

/// Mutable [`resolve`]: the path is walked by moving the borrow out at each step.
fn resolve_mut<'a>(root: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    let mut node = root;
    for step in path {
        let current = node;
        node = match step {
            Step::Key(key) => current.as_object_mut()?.get_mut(key.as_str())?,
            Step::Index(index) => current.as_array_mut()?.get_mut(*index)?,
        };
    }
    Some(node)
}

/// Collects every array in the payload, parents before children, remembering the path that reaches
/// it so a cut above can detach a nested list instead of accounting for bytes that are no longer in
/// the envelope.
fn collect_slots(value: &Value, path: &mut Vec<Step>, out: &mut Vec<Slot>) {
    match value {
        Value::Array(items) => {
            out.push(Slot {
                path: path.clone(),
                bytes: json_len(items),
            });
            for (index, item) in items.iter().enumerate() {
                path.push(Step::Index(index));
                collect_slots(item, path, out);
                path.pop();
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                path.push(Step::Key(key.clone()));
                collect_slots(item, path, out);
                path.pop();
            }
        }
        _ => {}
    }
}

/// How many items the array at `slot`'s path holds right now, or zero once a cut above detached it.
fn live_len(root: &Value, slot: &Slot) -> usize {
    resolve(root, &slot.path)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// The draft's payload, which is the root every payload slot path is relative to.
fn payload_of(draft: &mut Value) -> Option<&mut Value> {
    draft.get_mut("payload")
}

/// The draft's envelope-level skip list.
fn not_indexed_of(draft: &mut Value) -> Option<&mut Vec<Value>> {
    draft
        .get_mut("coverage")?
        .get_mut("not_indexed")?
        .as_array_mut()
}

/// How many tail items have to go to free `excess` bytes, and what that frees.
///
/// The sizes come from the list itself, so this is one pass over the items being dropped rather than
/// a re-serialisation of the whole envelope per removed row.
fn plan_tail_drop(items: &[Value], excess: i64) -> DropPlan {
    let sizes = item_sizes(items);
    let floor = list_bytes(&sizes) - EMPTY_LIST_BYTES;
    let mut saved = 0_u64;
    let mut count = 0_usize;
    while count < sizes.len() && (saved as i64) < excess {
        // A removed item takes the comma that separated it from its predecessor with it.
        saved += sizes[sizes.len() - 1 - count] + 1;
        count += 1;
    }
    DropPlan {
        count,
        saved: saved.min(floor),
    }
}

/// Drops `count` items from the end of the array at `path`.
fn apply_tail_drop(root: &mut Value, path: &[Step], count: usize) {
    if let Some(Value::Array(items)) = resolve_mut(root, path) {
        let keep = items.len().saturating_sub(count);
        items.truncate(keep);
    }
}

/// Trims `coverage.not_indexed` from the low-path end.
///
/// An entry whose path the payload already answers about is pinned to the last pass, so a request
/// that touched a skipped path keeps its reason. Pinned entries are only dropped when the clamped
/// ones cannot pay for the cut at all — the budget is the one guarantee that outranks them.
fn trim_coverage_tail(
    items: &mut Vec<Value>,
    excess: i64,
    pinned: &std::collections::HashSet<String>,
) -> DropPlan {
    let sizes = item_sizes(items);
    let floor = list_bytes(&sizes) - EMPTY_LIST_BYTES;
    let mut cut = vec![false; sizes.len()];
    let mut saved = 0_u64;
    let mut count = 0_usize;

    for pinned_pass in [false, true] {
        let mut cursor = sizes.len();
        while cursor > 0 && (saved as i64) < excess {
            cursor -= 1;
            if cut[cursor] {
                continue;
            }
            let path = items[cursor].get("path").and_then(Value::as_str);
            let is_pinned = path.is_some_and(|path| pinned.contains(path));
            if is_pinned != pinned_pass {
                continue;
            }
            cut[cursor] = true;
            saved += sizes[cursor] + 1;
            count += 1;
        }
        if (saved as i64) >= excess {
            break;
        }
    }

    if count > 0 {
        let kept: Vec<Value> = items
            .drain(..)
            .enumerate()
            .filter(|(index, _)| !cut[*index])
            .map(|(_, item)| item)
            .collect();
        *items = kept;
    }
    DropPlan {
        count,
        saved: saved.min(floor),
    }
}

/// Every string the payload carries. A `not_indexed` entry whose path is one of them is the reason
/// for a path the answer itself names, which is what "the request touched it" looks like from here.
fn paths_answered_by(value: &Value) -> std::collections::HashSet<String> {
    fn walk(value: &Value, depth: usize, seen: &mut std::collections::HashSet<String>) {
        if depth > MAX_PIN_DEPTH {
            return;
        }
        match value {
            Value::String(text) => {
                seen.insert(text.clone());
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, depth + 1, seen);
                }
            }
            Value::Object(map) => {
                for item in map.values() {
                    walk(item, depth + 1, seen);
                }
            }
            _ => {}
        }
    }

    let mut seen = std::collections::HashSet::new();
    walk(value, 0, &mut seen);
    seen
}

/// One target-driven pass: free `excess` bytes by cutting ranked payload lists first (the largest
/// one, lowest-ranked tail item first), then the envelope-level coverage explanation.
///
/// Lists are cut parents-before-children because a parent's serialised size always contains its
/// children's, and a nested list is skipped once the item that held it has been dropped.
fn trim_once(
    draft: &mut Value,
    payload_slots: &[Slot],
    excess: i64,
    pinned: &std::collections::HashSet<String>,
) -> DropPlan {
    let mut remaining = excess;
    let mut count = 0_usize;
    let mut saved = 0_u64;

    if let Some(payload) = payload_of(draft) {
        // Largest list first; ties go to the longer one, so the choice is deterministic.
        let mut ranked: Vec<(&Slot, usize)> = payload_slots
            .iter()
            .map(|slot| (slot, live_len(payload, slot)))
            .collect();
        ranked.sort_by(|(left, left_len), (right, right_len)| {
            right
                .bytes
                .cmp(&left.bytes)
                .then_with(|| right_len.cmp(left_len))
        });

        for (slot, _) in ranked {
            if remaining <= 0 {
                break;
            }
            let Some(Value::Array(items)) = resolve(payload, &slot.path) else {
                continue;
            };
            if items.is_empty() {
                continue;
            }
            let plan = plan_tail_drop(items, remaining);
            if plan.count == 0 {
                continue;
            }
            apply_tail_drop(payload, &slot.path, plan.count);
            count += plan.count;
            saved += plan.saved;
            remaining -= plan.saved as i64;
        }
    }

    if remaining > 0
        && let Some(items) = not_indexed_of(draft).filter(|items| !items.is_empty())
    {
        let plan = trim_coverage_tail(items, remaining, pinned);
        count += plan.count;
        saved += plan.saved;
    }

    DropPlan { count, saved }
}

/// The byte allowance a token budget buys, refusing a budget above [`MAX_BUDGET_TOKENS`].
///
/// The TypeScript version also refuses a non-finite budget; an integer argument cannot be
/// non-finite, so the ceiling is the only refusal here.
///
/// # Errors
/// `budget_too_large` when `budget_tokens` exceeds [`MAX_BUDGET_TOKENS`].
pub fn budget_byte_allowance(budget_tokens: u64) -> Result<u64, AppError> {
    if budget_tokens > MAX_BUDGET_TOKENS {
        return Err(AppError::new(
            ErrorCode::BUDGET_TOO_LARGE,
            format!("budget {budget_tokens} exceeds the {MAX_BUDGET_TOKENS} token ceiling"),
        )
        .with_hint(format!("request at most {MAX_BUDGET_TOKENS} tokens")));
    }
    Ok(budget_tokens * BYTES_PER_TOKEN)
}

/// The single place a payload is truncated.
///
/// `mint` is called exactly once, only when something was actually dropped, with the number of
/// items (payload and coverage) that left the envelope. A payload with no lists to shrink and no
/// coverage to abridge cannot be cut, so it is returned unchanged rather than mislabelled as
/// truncated. An envelope whose own skeleton already exceeds the allowance is the same case from
/// the other side — the budget is below the floor the required fields cost — so it is returned
/// untouched too: no false `truncated`, and no token pointing at a page that cannot advance.
/// A caller-flagged truncation (`depth`, `page`, `item_cap`) survives that floor, with `eq`
/// downgraded to `gte`.
///
/// # Errors
/// `budget_too_large` when `budget_tokens` exceeds [`MAX_BUDGET_TOKENS`]; `internal` when the
/// envelope cannot be serialised, which no contract type can trigger.
pub fn emit<T>(
    env: Envelope<T>,
    budget_tokens: u64,
    mut mint: impl FnMut(usize) -> String,
) -> Result<Envelope<T>, AppError>
where
    T: Serialize + DeserializeOwned,
{
    /// The envelope as it arrived, except that a payload flagged as truncated may never claim an
    /// exact total even if it fits. Only [`emit`] calls this: the downgrade is emit's decision, not
    /// a policy of its own.
    fn unless_flagged<T>(envelope: Envelope<T>) -> Envelope<T> {
        if !envelope.truncated || envelope.totals.relation() == TotalRelation::Gte {
            return envelope;
        }
        let totals = envelope.totals.clone().with_relation(TotalRelation::Gte);
        with_totals(envelope, totals)
    }

    let budget_bytes = budget_byte_allowance(budget_tokens)?;
    let mut draft = serde_json::to_value(&env).map_err(|error| {
        AppError::internal(format!("the envelope is not serialisable: {error}"))
    })?;
    if json_len(&draft) <= budget_bytes {
        return Ok(unless_flagged(env));
    }

    // Read from the caller's payload, not from the draft: a payload list is drained before the
    // coverage list is touched, so the answer's own mention of a path can be gone by then — and the
    // pin exists precisely to survive that cut. Nothing has been cut yet here, so the copy is the
    // caller's payload verbatim.
    let pinned = draft
        .get("payload")
        .map_or_else(std::collections::HashSet::new, paths_answered_by);

    // The forced shape while planning: `truncated`, `budget`, a placeholder for the token that has
    // not been minted yet (it charges the measurement for the reserve), and `gte`.
    draft["truncated"] = Value::Bool(true);
    draft["truncation_reason"] = Value::String(TruncationReason::Budget.as_str().to_owned());
    draft["next"] = Value::String("x".repeat(NEXT_TOKEN_RESERVE_CHARS));
    draft["totals"]["total_relation"] = Value::String(TotalRelation::Gte.as_str().to_owned());

    let mut payload_slots = Vec::new();
    if let Some(payload) = draft.get("payload") {
        collect_slots(payload, &mut Vec::new(), &mut payload_slots);
    }

    let mut dropped = 0_usize;
    let mut measured = json_len(&draft);
    for _ in 0..MAX_TRIM_PASSES {
        if measured <= budget_bytes {
            break;
        }
        let plan = trim_once(
            &mut draft,
            &payload_slots,
            (measured - budget_bytes) as i64,
            &pinned,
        );
        if plan.count == 0 {
            break;
        }
        dropped += plan.count;
        measured = json_len(&draft);
    }

    if dropped == 0 {
        return Ok(unless_flagged(env));
    }

    // The floor: every droppable list is gone and the envelope still does not fit, so the budget is
    // below what the required fields alone cost. A drained copy labelled `truncated` would claim a
    // cut the caller can act on while still being over its allowance, and the minted position would
    // be the one the call came in with — the head of a page that cannot advance. The honest answer
    // is the envelope as it arrived: unchanged, unlabelled, and with no token `mint` was never
    // asked for.
    if measured > budget_bytes {
        return Ok(unless_flagged(env));
    }

    let mut emitted: Envelope<T> = serde_json::from_value(draft).map_err(|error| {
        AppError::internal(format!("the trimmed envelope is not an envelope: {error}"))
    })?;
    emitted.next = Some(mint(dropped));
    Ok(emitted)
}

/// The smallest budget this answer can be delivered at: its serialised bytes at the declared
/// exchange rate, rounded up.
///
/// [`emit`] returns an answer untouched when the budget is below this — the floor the required
/// fields cost — and a tool refuses then instead of serving it, because a response many times the
/// caller's allowance is not an answer to its budget.
#[must_use]
pub fn budget_floor_of<T: Serialize + ?Sized>(value: &T) -> u64 {
    json_len(value).div_ceil(BYTES_PER_TOKEN)
}
