//! `search`: BM25 over the symbol index (`TOOLS-SPEC.md` §5).
//!
//! Ported from `src/tools/search/handler.ts` (373 lines). The handler is the tool's whole policy and
//! does no work of its own: the ranked read is [`GraphReader::read_node_fts`], the degree join is
//! [`GraphReader::metrics_for`], the absence arithmetic is `sangkar_domain::coverage`, sizing is
//! [`sangkar_core::budget::emit`] and the envelope comes from [`sangkar_core::envelope`]. What is
//! decided here is:
//!
//! - **a page boundary is stable.** The reader returns FTS hits already ordered by `bm25` ascending,
//!   then `in_degree` descending, then `id` ascending, so the tool never re-sorts: a re-sort is how a
//!   cursor starts repeating or skipping rows, and degree is only a *tie-breaker inside the reader's
//!   ordering*, not a second ranking here. A cursor's `position` is therefore an index into the
//!   flattened, already-filtered hit list, and page two is the same list sliced one item later.
//! - **the caller's text reaches FTS5 unchanged.** `src/tools/search/handler.ts:301` binds the
//!   trimmed query straight to `MATCH`, so `widget*` is a prefix query and `a OR b` is a
//!   disjunction; [`match_query_of`] reproduces that rather than quoting, because a search that
//!   answers differently after the migration is the failure this port exists to avoid. A malformed
//!   expression fails inside SQLite and is carried out as the same `internal` envelope the
//!   TypeScript returns, so no raw SQL error escapes. An empty result is `not_found`, never an error.
//! - **`mode` is disclosed rather than implied.** An agent that asked something semantic and got
//!   keyword order should know before it trusts the ranking (§5).
//! - **`score` is the raw `bm25()` value**, where smaller is better. It is not converted, rescaled or
//!   rounded; [`Bm25Value`] only fixes its *spelling* so an integral score renders as `0` and not
//!   `0.0`, which is what `JSON.stringify` writes.
//! - **the degree join is a disambiguator, not an absence claim.** `metrics_for` returning `None`
//!   means the publish never measured the node; a hit reads `0/0` and the *absence* questions stay
//!   with the tools that gate on coverage.
//! - **a failed read is an error envelope, never `not_found`.** `meta_for`, the FTS read, the metrics
//!   join and `find_by_qn` all return `Result`; a failed read must not become "nothing matched", so
//!   [`handle`] answers it with [`fail`] and the error's own code.
//!
//! Two porting decisions are visible in the code:
//!
//! - **the argument digest is bare hex.** `argsHashOf` in `handler.ts` hashes the compact JSON of
//!   `[query, kind, prefix]` and returns `digest("hex")` with no `sha256:` prefix, unlike the other
//!   tools; that inconsistency is the wire contract the captured goldens carry and is asserted by
//!   [`args_hash_of`]'s test.
//! - **`in` is spelled `in_` in Rust.** The payload key stays `in` (`#[serde(rename)]`): the field
//!   name is not the wire name, and a client diffing two answers sees the documented object.

use sha2::{Digest, Sha256};

use sangkar_core::budget::{budget_floor_of, emit};
use sangkar_core::cursor::{CursorExpectation, CursorPayload, CursorRefusal, CursorResult};
use sangkar_core::envelope::{
    BootstrappingSpec, Coverage, Envelope, EnvelopeSpec, ReadStatus, TestCensus, TotalRelation,
    Totals, bootstrapping, fail, not_found, ok,
};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::instructions::next_tools_for;
use sangkar_core::paths::{is_under_prefix, normalize_rel};
use sangkar_domain::coverage::{CoverageScope, CoverageSource, assemble, coverage_source};
use sangkar_store::read::context::ToolContext;
use sangkar_store::read::coverage::{PathCoverage, UnresolvedRef};
use sangkar_store::read::reader::{FtsHit, GraphReader};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::search::manifest::{NAME, SearchArgs};

/// The statement bound handed to [`GraphReader::read_node_fts`], not a page size.
///
/// The reader fixes `limit` as a bound on how many rows the statement may return; the product
/// decision about how many are shown belongs to [`emit`]. 200 is ten times the ~20-hit page a default
/// budget holds, so the cut a caller sees is a budget cut with a continuation. A query with more than
/// 200 matches is the one case the bound rather than the budget ends the list, and the summary says
/// so instead of pretending the 200th hit was the last one.
pub const READER_BOUND: usize = 200;

/// The payload's `mode`. Named once because the summary and the payload must not disagree.
const MODE: &str = "bm25";

/// The index has not published a generation yet, so there is no graph to search. Answering with an
/// empty result list would be a false absence claim; the caller is told to retry instead.
const BOOTSTRAP_RETRY_MS: u64 = 1_000;

const UNPUBLISHED_SUMMARY: &str = "The index has not published a generation yet.";

const UNPUBLISHED_HINT: &str =
    "re-run once the indexer has published; `status` reports the progress";

/// The `stale_reason` of an answer whose read failed, in the words `src/index.ts`'s
/// `uncheckedReadStatus` uses for the same situation.
const READ_FAILED: &str = "the read that would have answered it failed";

/// In/out degree, the cheapest disambiguator a ranked list can carry (§5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchDegree {
    /// `in_degree` under its wire name: `in` is a Rust keyword, the payload key is not.
    #[serde(rename = "in")]
    pub in_: i64,
    pub out: i64,
}

/// One declaration. Never source text: same rationale as `summary` (§5 "No source").
///
/// `signature` is declared last on purpose. `handler.ts` builds the base object and spreads
/// `signature` onto it, so the key lands *after* `score`; the captured golden carries that order and
/// serde's declaration order is what reproduces it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    pub id: i64,
    pub qn: String,
    pub kind: String,
    pub file: String,
    pub lines: [i64; 2],
    pub degree: SearchDegree,
    pub score: Bm25Value,
    /// Present only when the row has one — never `null`, because a key that vanished and a key
    /// holding `null` are different payloads to a client diffing two answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// The one payload this tool produces. `mode` travels with the rows rather than being implied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchPayload {
    pub mode: String,
    pub results: Vec<SearchResult>,
}

/// `ok`/`not_found`/`error`/`bootstrapping` all carry this payload (an error carries `None`).
pub type SearchEnvelope = Envelope<SearchPayload>;

/// A `bm25()` value, serialised so an integral score loses the `.0`.
///
/// `JSON.stringify(0)` is `0`; serde_json's shortest-round-trip writer for `0.0f64` is `0.0`. A real
/// bm25 score is rarely integral, which is why this wrapper is not a rounding step: it writes a whole
/// number only when the value already is one (and inside `±Number.MAX_SAFE_INTEGER`, past which an
/// `f64` cannot represent every integer). The value itself — smaller is better, never converted — is
/// what [`Self::value`] returns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bm25Value(f64);

impl Bm25Value {
    /// A score exactly as the reader computed it.
    #[must_use]
    pub const fn from_f64(value: f64) -> Self {
        Self(value)
    }

    /// The number itself, for a comparison that is not about its spelling.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }

    /// The value as an integer, when it renders without a fractional part.
    ///
    /// `None` for `NaN`/infinite values (which `serde_json` and `JSON.stringify` both write as
    /// `null`) and for anything with a fraction.
    fn integral(self) -> Option<i64> {
        if self.0.is_finite() && self.0.fract() == 0.0 && self.0.abs() <= MAX_EXACT_INTEGER {
            Some(self.0 as i64)
        } else {
            None
        }
    }
}

/// `Number.MAX_SAFE_INTEGER`: past it an `f64` cannot represent every integer, so writing one would
/// claim a precision the value does not have.
const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_991.0;

impl Serialize for Bm25Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.integral() {
            Some(whole) => serializer.serialize_i64(whole),
            None => serializer.serialize_f64(self.0),
        }
    }
}

impl<'de> Deserialize<'de> for Bm25Value {
    /// A payload from another process or from [`emit`]'s own round trip: the number is a number, and
    /// the spelling is this wrapper's to fix again on the way out.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        f64::deserialize(deserializer).map(Self)
    }
}

/// The FTS5 match expression for a caller's query — the trimmed text, unchanged.
///
/// This is a PARITY decision, not a missing sanitiser. `src/tools/search/handler.ts:301` binds the
/// caller's trimmed query straight to `MATCH`, so FTS5 reads it as syntax: `widget*` is a prefix
/// query that WIDENS the match set, `a OR b` is a disjunction, `a:b` is a column filter that fails
/// with "no such column", and a lone `"` is an unterminated string. Reproducing the TypeScript's
/// answers means reproducing that, so this function deliberately does not quote.
///
/// An earlier revision quoted every term. That made caller text unable to become FTS syntax, which
/// sounds safer, but it turned `widget*` from a prefix query into the literal token `widget*`, so one
/// call returned different rows in the two binaries. The maintainer ruled for parity: a search that
/// answers differently after the migration is the failure this port exists to avoid.
///
/// A malformed expression therefore reaches SQLite and fails there, and the handler turns that into
/// the same `internal` envelope `handler.ts` returns — the error is caught at the reader seam, so it
/// never escapes as a raw SQL error or a panic. Trimmed before this point by [`handle`], and an
/// all-whitespace query is answered as an empty search before any read, because an empty expression
/// is not valid `MATCH` syntax.
#[must_use]
pub fn match_query_of(query: &str) -> String {
    query.to_owned()
}

/// The `argsHash` a cursor minted by this tool is pinned to: the bare hex digest of the compact JSON
/// of `[query, kind, prefix]` (`handler.ts:128-133`).
///
/// `kind` is `null` when the caller did not ask, and `prefix` is the *normalised* path, so `--path
/// ./src/` and `--path src` hash the same and `budget`/`cursor` are deliberately absent — they decide
/// how much of the same list is shown, not which rows are in it.
///
/// Public because it is wire contract, not an internal detail: the cursor a truncated answer mints
/// carries this exact string, and `tests/search.rs` pins it against the digest inside the captured
/// golden's own token — the one place the TypeScript's answer is recorded byte for byte.
#[must_use]
pub fn args_hash_of(query: &str, kind: Option<&str>, prefix: &str) -> String {
    let canonical = Value::Array(vec![
        Value::String(query.to_owned()),
        kind.map_or(Value::Null, |kind| Value::String(kind.to_owned())),
        Value::String(prefix.to_owned()),
    ]);
    let digest = Sha256::digest(canonical.to_string().as_bytes());
    let mut hex = String::with_capacity(64);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

/// The optional filters, applied after the read and without touching the order.
///
/// [`is_under_prefix`] is what keeps `src` from matching `src-old`, and an empty prefix is the whole
/// tree rather than a prefix of nothing.
fn matches(hit: &FtsHit, kind: Option<&str>, prefix: &str) -> bool {
    if let Some(kind) = kind
        && hit.kind != kind
    {
        return false;
    }
    prefix.is_empty() || is_under_prefix(&hit.file, prefix)
}

/// One payload row. `metrics_for` returning `None` means the publish never measured the node; the
/// documented payload still requires two integers, and a search hit is a disambiguator rather than an
/// absence claim, so an unmeasured node reads `0/0` here.
///
/// The signature is one unique-index lookup per candidate, and candidates are sliced to the page
/// window first, because [`FtsHit`] carries identity and rank only.
fn to_result(reader: &dyn GraphReader, hit: &FtsHit) -> Result<SearchResult, AppError> {
    let metrics = reader.metrics_for(hit.id)?;
    let signature = reader
        .find_by_qn(&hit.qn)?
        .and_then(|row| row.signature)
        .filter(|text| !text.is_empty());
    Ok(SearchResult {
        id: hit.id,
        qn: hit.qn.clone(),
        kind: hit.kind.clone(),
        file: hit.file.clone(),
        lines: [hit.lines.start, hit.lines.end],
        degree: SearchDegree {
            in_: metrics.as_ref().map_or(0, |metrics| metrics.in_degree),
            out: metrics.as_ref().map_or(0, |metrics| metrics.out_degree),
        },
        score: Bm25Value::from_f64(hit.score),
        signature,
    })
}

/// The one line a model can quote (§1.2). "showing N of M" is a range once a page starts past the
/// first item, and `capped` says so plainly when the statement bound, rather than the budget, is what
/// ended the candidate set.
fn summary_for(query: &str, total: usize, start: u64, shown: usize, capped: bool) -> String {
    let head = if total == 1 {
        format!("Found 1 declaration for '{query}'")
    } else {
        format!("Found {total} declarations for '{query}'")
    };
    let cap = if capped {
        format!(" among the {READER_BOUND} best matches")
    } else {
        String::new()
    };
    if shown == 0 {
        return format!("{head}{cap}; nothing further from position {start}.");
    }
    let first = start + 1;
    let last = start + u64::try_from(shown).unwrap_or(u64::MAX);
    let showing = if first == 1 && last >= total as u64 {
        format!("{total} of {total}")
    } else {
        format!("{first}-{last} of {total}")
    };
    format!("{head}{cap}, showing {showing}.")
}

/// The query matched, the filters did not: saying which one emptied the list is the difference.
fn filtered_out_summary(query: &str, matched: usize) -> String {
    format!(
        "No declarations matched '{query}' under the requested kind or path filter, \
         though {matched} matched the query alone."
    )
}

/// Everything an answer says about the generation it was served from and the files it covers.
struct Scope {
    read_status: ReadStatus,
    coverage: Coverage,
    relation: TotalRelation,
}

/// The rows [`scope_of`] already read, handed to [`assemble`] so one scope costs one lookup.
///
/// It is not a second database adapter: `rows_for_paths` answers from the rows it was built with and
/// the other three methods forward to the reader, which is exactly what `handler.ts` does with an
/// object literal around the same pre-read rows.
struct ReadScope<'a> {
    rows: PathCoverage,
    reader: &'a dyn GraphReader,
}

impl CoverageSource for ReadScope<'_> {
    fn rows_for_paths(&self, _prefixes: Option<&[String]>) -> Result<PathCoverage, AppError> {
        Ok(self.rows.clone())
    }

    fn unresolved_for_files(&self, file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
        self.reader.unresolved_for_files(file_ids)
    }

    fn not_indexed_count(&self) -> Result<i64, AppError> {
        self.reader.not_indexed_count()
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        self.reader.tests_census()
    }
}

/// Scope, freshness and coverage from one `rows_for_paths` read.
///
/// The staleness verdict is the graph-side one `coverage` and `status` use: a file whose
/// `indexed_generation` is not the served generation makes the answer stale even when the working
/// tree has not moved, and a handler has no filesystem to compare bytes against.
///
/// `stale_checked_at` is claimed only when there was a file to look at: "we did not look" and "we
/// looked and it is current" are different statements. A repo-wide search passes no prefix, so
/// `rows_for_paths` has nothing to answer — `complete: false` and `gte` are the whole verdict, exactly
/// as for a scope the indexer never saw.
fn scope_of(
    ctx: &ToolContext,
    generation: &str,
    indexed_at: &str,
    scope_paths: &[String],
) -> Result<Scope, AppError> {
    let rows = ctx.reader.rows_for_paths(scope_paths)?;
    let stale_files = rows
        .files
        .iter()
        .filter(|file| file.indexed_generation != generation)
        .count();
    let stale = stale_files > 0;
    let mut read_status = ReadStatus {
        generation: generation.to_owned(),
        indexed_at: indexed_at.to_owned(),
        stale,
        stale_reason: None,
        stale_checked_at: if rows.files.is_empty() {
            None
        } else {
            Some((ctx.now)())
        },
        decisive: None,
    };
    if stale {
        read_status.stale_reason = Some(format!(
            "{stale_files} of {} file(s) in scope were written by an earlier generation",
            rows.files.len()
        ));
    }
    let assembly = assemble(
        &ReadScope {
            rows,
            reader: ctx.reader.as_ref(),
        },
        &CoverageScope {
            paths: Some(scope_paths.to_vec()),
            generation: generation.to_owned(),
            stale,
            file_ids: None,
            include_tests: false,
        },
    )?;
    Ok(Scope {
        read_status,
        coverage: assembly.coverage,
        relation: assembly.relation,
    })
}

/// No scope was checked, so nothing may be claimed: `complete: false` is the whole answer.
fn unscoped_coverage() -> Coverage {
    Coverage {
        complete: false,
        indexed: 0,
        skipped: 0,
        failed: 0,
        unresolved_edges: 0,
        not_indexed: Vec::new(),
        tests: None,
    }
}

/// The envelope of a read that failed: the error's own code and message, and a `read_status` that
/// claims nothing (an empty generation, `stale: true` with the reason) — a failed read is not an
/// absence claim, so it must not read as `not_found` either.
fn read_failure(error: AppError, query: &str) -> SearchEnvelope {
    fail(
        error,
        EnvelopeSpec::new(
            format!("search '{query}': the answer could not be read"),
            ReadStatus {
                generation: String::new(),
                indexed_at: String::new(),
                stale: true,
                stale_reason: Some(READ_FAILED.to_owned()),
                stale_checked_at: None,
                decisive: None,
            },
            unscoped_coverage(),
        )
        .with_next_tools(next_tools_for(NAME)),
    )
}

/// Not `not_found`: an empty database is no evidence that nothing matches, and answering `not_found`
/// would let the agent tell the user the name does not exist.
fn unpublished(ctx: &ToolContext) -> Result<SearchEnvelope, AppError> {
    // The scope of the absence claim is the empty one, assembled the way `handler.ts` does, so the
    // coverage block is this repository's answer to "under no prefix" rather than a hardcoded zero.
    let coverage = assemble(
        &coverage_source(ctx.reader.as_ref()),
        &CoverageScope {
            paths: Some(Vec::new()),
            generation: String::new(),
            stale: true,
            file_ids: None,
            include_tests: false,
        },
    )?
    .coverage;
    Ok(bootstrapping(
        BootstrappingSpec::new(
            EnvelopeSpec::new(
                UNPUBLISHED_SUMMARY,
                ReadStatus {
                    // The empty id is the truthful value: a fabricated one would give a cursor
                    // something to bind to that no publish will ever match.
                    generation: String::new(),
                    indexed_at: (ctx.now)(),
                    stale: true,
                    stale_reason: None,
                    stale_checked_at: None,
                    decisive: None,
                },
                coverage,
            )
            .with_next_tools(next_tools_for(NAME)),
            BOOTSTRAP_RETRY_MS,
        )
        .with_hint(UNPUBLISHED_HINT),
    ))
}

/// A partial page is never served: the refusal carries the cursor module's own code and hint (a
/// handler that re-worded them could tell an agent to guess a page instead of re-running the call),
/// and the answer's scope, because a refusal still describes what was asked about.
fn cursor_failure(refusal: CursorRefusal, scope: &Scope) -> SearchEnvelope {
    fail(
        AppError::new(
            refusal.code,
            format!("the cursor was not accepted: {}", refusal.code),
        )
        .with_hint(refusal.hint),
        EnvelopeSpec::new(
            format!(
                "Cursor refused ({}); re-run the search for the first page.",
                refusal.code
            ),
            scope.read_status.clone(),
            scope.coverage.clone(),
        )
        .with_next_tools(next_tools_for(NAME)),
    )
}

/// An empty query has nothing to match, and asking FTS5 for `MATCH ''` would be a syntax error rather
/// than an answer: the miss is reported without a read.
fn empty_query(scope: Scope) -> SearchEnvelope {
    not_found(
        SearchPayload {
            mode: MODE.to_owned(),
            results: Vec::new(),
        },
        EnvelopeSpec::new(
            "An empty query has nothing to match; give search a word or a name.".to_owned(),
            scope.read_status,
            scope.coverage,
        )
        .with_totals(Totals::new(scope.relation).with_total("results", Some(0)))
        .with_next_tools(next_tools_for(NAME)),
    )
}

/// Answers one `search` call. The order of the steps is part of the contract: a cursor is verified
/// against `{generation, tool, args_hash}` *before* the search work, so a stale or foreign token can
/// never be served a page from this call.
fn answer(ctx: &ToolContext, args: &SearchArgs) -> Result<SearchEnvelope, AppError> {
    // The generation is the precondition of everything below — a cursor binds to it and `read_status`
    // must name it — so this one meta read happens before the cursor check, not as search work.
    let meta = ctx.reader.meta_for()?;
    let Some(generation) = meta.generation.clone() else {
        return unpublished(ctx);
    };
    let indexed_at = ctx
        .reader
        .latest_generation()?
        .map_or_else(String::new, |row| row.published_at);

    let query = args.query.trim();
    let prefix = normalize_rel(args.path.as_deref().unwrap_or(""));
    let scope_paths: Vec<String> = if prefix.is_empty() {
        Vec::new()
    } else {
        vec![prefix.clone()]
    };
    let args_hash = args_hash_of(query, args.kind.as_deref(), &prefix);
    // The envelope's own scope reads (freshness, coverage) happen before the cursor check because a
    // refusal envelope must carry them too; the *search* — the MATCH and its ranking — does not.
    let scope = scope_of(ctx, &generation, &indexed_at, &scope_paths)?;

    let mut start = 0_u64;
    if let Some(token) = args.cursor.as_deref().filter(|token| !token.is_empty()) {
        let expected = CursorExpectation {
            generation: generation.clone(),
            tool: NAME.to_owned(),
            args_hash: args_hash.clone(),
        };
        match ctx.cursor.verify(token, &expected) {
            CursorResult::Accepted { position } => start = position,
            CursorResult::Refused(refusal) => return Ok(cursor_failure(refusal, &scope)),
        }
    }

    if query.is_empty() {
        return Ok(empty_query(scope));
    }

    // The bound is the reader's, not a page size: it says how many rows the statement may return, and
    // `emit` decides how many of those are shown.
    let hits = ctx
        .reader
        .read_node_fts(&match_query_of(query), READER_BOUND)?;
    let matched = hits.len();
    let capped = matched == READER_BOUND;
    let filtered: Vec<FtsHit> = hits
        .into_iter()
        .filter(|hit| matches(hit, args.kind.as_deref(), &prefix))
        .collect();
    // `slice(start)` on a position past the end is an empty page, not a panic: a signed cursor can
    // address a list that shrank between two calls.
    let window: &[FtsHit] = usize::try_from(start)
        .ok()
        .and_then(|skip| filtered.get(skip..))
        .unwrap_or_default();
    let results = window
        .iter()
        .map(|hit| to_result(ctx.reader.as_ref(), hit))
        .collect::<Result<Vec<_>, _>>()?;

    let totals = Totals::new(scope.relation).with_total("results", Some(filtered.len() as u64));
    let spec = EnvelopeSpec::new(
        if filtered.is_empty() && matched > 0 {
            filtered_out_summary(query, matched)
        } else {
            summary_for(query, filtered.len(), start, results.len(), capped)
        },
        scope.read_status,
        scope.coverage,
    )
    // The routing table, not this handler's own guess: `src/index.ts`'s `route()` republishes the
    // same list, so the envelope a unit test sees is the envelope the surface serves.
    .with_totals(totals)
    .with_next_tools(next_tools_for(NAME));

    let payload = SearchPayload {
        mode: MODE.to_owned(),
        results,
    };
    let envelope = if filtered.is_empty() {
        not_found(payload, spec.clone())
    } else {
        ok(payload, spec.clone())
    };

    let budget = args.budget_tokens();
    let window_len = window.len();
    let generation_for_page = generation.clone();
    let hash_for_page = args_hash.clone();
    let mint = |dropped: usize| {
        // The kept count is `window.len() - dropped`, so the resumed page starts exactly where this
        // one stopped. The floor at zero is there because `emit` keeps going after the results list
        // is empty (it then trims envelope-level lists), and a signed cursor may never carry a
        // negative position.
        let kept = u64::try_from(window_len.saturating_sub(dropped)).unwrap_or(0);
        ctx.cursor.sign(&CursorPayload {
            generation: generation_for_page.clone(),
            tool: NAME.to_owned(),
            args_hash: hash_for_page.clone(),
            position: start + kept,
        })
    };
    let emitted = match emit(envelope, budget, mint) {
        Ok(emitted) => emitted,
        // `budget_too_large` is a client error (TOOLS-SPEC.md §9), and a handler returns envelopes.
        Err(error) => return Ok(fail(error, spec)),
    };

    // A zero-row page carrying a continuation token would be an infinite loop, and a budget below the
    // floor this answer needs cannot be cut to fit at all (`emit` then returns the envelope untouched
    // rather than mislabelling a cut). Both are the refusal the caller can act on.
    let floor = budget_floor_of(&emitted);
    let shown = emitted
        .payload
        .as_ref()
        .map_or(0, |payload| payload.results.len());
    if window_len > 0 && (shown == 0 || floor > budget) {
        return Ok(fail(
            AppError::new(
                ErrorCode::BUDGET_TOO_LARGE,
                format!("budget {budget} fits none of the {window_len} remaining declarations"),
            )
            .with_hint(format!(
                "raise the budget; this answer needs at least {floor} tokens"
            )),
            spec,
        ));
    }

    // The cut belongs to `emit`, so the exact "showing N of M" count exists only after it runs: the
    // summary is corrected on the copy it returns rather than guessed before the cut.
    if !emitted.truncated {
        return Ok(emitted);
    }
    let mut corrected = emitted;
    corrected.summary = summary_for(query, filtered.len(), start, shown, capped);
    Ok(corrected)
}

/// One free-text query in, one envelope out (`TECH-SPEC.md` §6: handlers return envelopes, never
/// throw, never read the environment and never open a database).
///
/// A store read that fails is an error envelope with the error's own code, never `not_found`: a failed
/// read is not an absence claim.
#[must_use]
pub fn handle(ctx: &ToolContext, args: &SearchArgs) -> SearchEnvelope {
    match answer(ctx, args) {
        Ok(envelope) => envelope,
        Err(error) => read_failure(error, args.query.trim()),
    }
}
