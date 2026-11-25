//! `status`: is the graph current, and what does it cover (`TOOLS-SPEC.md` §2)?
//!
//! Ported from `src/tools/status/handler.ts`. The trust question, answered from the reader alone.
//! Three facts make the answer usable and each is reported even when it is bad news:
//! `coverage.complete` plus `coverage.unresolved_edges` gate every later absence claim; the language
//! census reports a language that parsed nothing as `parsed: 0` with a note instead of dropping it;
//! and a requested path that is in neither `file` nor `not_indexed` comes back as
//! `not_indexed_unknown` rather than as a zero.
//!
//! Four limits are deliberate and visible in the payload:
//!
//! - `read_status` is the verdict of [`sangkar_domain::freshness`], not a reading of `meta`: the
//!   graph-side half survives (a `file` row written by an earlier generation than the one being
//!   served) and the filesystem-side half is added — a `project_root` mismatch, a base commit that
//!   is no longer `HEAD`, a watcher busy with nothing to publish, and a digest that no longer
//!   matches the bytes on disk. `stale_checked_at` is that module's own clock, so the claim is dated
//!   rather than absolute. This handler only decides how to word the reason it is handed.
//! - `graph.db_path` and `graph.size_bytes`: [`sangkar_store::read::reader::GraphReader`] exposes no
//!   database path, so the path is derived from `ctx.config.data_dir` — never from the environment —
//!   and the size is the one recorded when the reader opened that file (`ctx.db_size_bytes`). The
//!   TypeScript's `stat` fallback has no counterpart here: the Rust context carries a `u64`, not an
//!   optional number, because a context exists only around an open handle.
//! - `coverage` is two measurements, not one. `coverage.skipped` counts exactly the rows
//!   `coverage.not_indexed` lists, so the two can never describe two populations; `indexed` and
//!   `failed` are the repository's, because a tool that reported a scope's file count as "the graph"
//!   would let a narrow request understate what was indexed. The repository's own skip count
//!   travels beside it as `totals.not_indexed_total`, which is why the two numbers may differ
//!   without either lying. [`sangkar_domain::coverage::assemble`] supplies the scoped rows, the
//!   unresolved census and the tests census; this module re-states the two repo-wide counts it does
//!   not.
//! - `payload.unresolved` is the bucket breakdown of the flat `coverage.unresolved_edges` count,
//!   taken from the one row set that produced it: `total`, `sum(buckets)` and `unresolved_edges` are
//!   the same measurement read once.
//!
//! `health.statement_cache_entries` is [`sangkar_domain::freshness::health`]'s process-wide count,
//! not a per-handle one — the reader trait is deliberately statements only — so a run that has
//! opened more than one handle reports their sum. `tests/parity/README.md` lists the field as
//! per-run, which is why the goldens compare it by shape and the tests here assert it is a *live*
//! count rather than a fixed number.
//!
//! The one failure mode is a reader that cannot answer: every `GraphReader` method returns a
//! `Result`, and a failed query becomes an `error` envelope on an unscoped skeleton with
//! `read_status.stale` set — not an `ok` envelope carrying a count no query produced. Nothing
//! leaves this module as a `Result`, so a surface has one call to make and one shape to render; a
//! *budget* refusal is likewise the contract's `status: error` envelope, with the code and the hint
//! that says what to change.

use std::collections::HashSet;

use sangkar_core::budget::{budget_floor_of, emit};
use sangkar_core::config::RepoConfigSource;
use sangkar_core::cursor::CursorPayload;
use sangkar_core::envelope::{
    BootstrappingSpec, Coverage, Envelope, EnvelopeSpec, ReadStatus, Totals, bootstrapping, fail,
    ok, relation_for,
};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::instructions::next_tools_for;
use sangkar_core::paths::{is_under_prefix, normalize_rel};
use sangkar_domain::coverage::{CoverageScope, assemble, coverage_source};
use sangkar_domain::freshness::{Freshness, FreshnessOptions, ToolHealth, check_freshness, health};
use sangkar_domain::unresolved::{UnresolvedBucket, UnresolvedCensus, bucket_census};
use sangkar_store::read::context::ToolContext;
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow};
use sangkar_store::read::files::LanguageCensus;
use sangkar_store::read::reader::{GenerationRow, GraphMeta};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::status::manifest::{NAME, STATUS_DEFAULT_TOKENS, STATUS_RETRY_AFTER_MS, StatusArgs};

/// The reason a requested path carries when neither half of the coverage answer mentions it.
const REASON_NOT_INDEXED_UNKNOWN: &str = "not_indexed_unknown";

/// The `note` a language gets when it contributed no parse at all (`TOOLS-SPEC.md` §2).
const NOTE_GRAMMAR_UNAVAILABLE: &str = "grammar_unavailable";

/// The summary of the answer an unpublished database gets.
const UNPUBLISHED_SUMMARY: &str =
    "No generation has been published yet, so there is no graph to report on.";

/// The `error.message` of that answer, byte for byte the TypeScript's.
const UNPUBLISHED_MESSAGE: &str = "the index has not published a generation yet";

/// The `error.hint` of that answer: what an agent should do instead of retrying blindly.
const UNPUBLISHED_HINT: &str =
    "retry after retry_after_ms; if the indexer is not running, start it";

/// The `GraphMeta` stale reason of a read that failed before a verdict could be reached.
const UNREAD_ANSWER: &str = "the call failed before the graph answered";

/// The summary a budget refusal carries, because the report it describes was never delivered.
const REFUSED_SUMMARY: &str = "the requested budget could not be applied";

/// The `GraphMeta` stale reason of an unreadable graph.
const UNREADABLE_GENERATION: &str = "no published generation is readable";

/// One language of the census. `language` stays `None` for a skipped path whose grammar the indexer
/// could not name — the reader keeps that bucket rather than inventing a name for it — and `note` is
/// absent unless there is something to explain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusLanguage {
    pub language: Option<String>,
    pub files: i64,
    pub parsed: i64,
    pub failed: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One path in the answer. `indexed: false` always carries a `reason`, so "not indexed" is never a
/// bare `false`. `symbols` is present only when the request named the file: it costs one
/// `nodes_by_file` query, and a directory request can match hundreds of files, which would turn the
/// cheap tool into the most expensive one.
///
/// The field order is the wire order the TypeScript's object literals produce: an indexed entry is
/// `path, indexed, sha256` with `symbols` appended, a skipped one is `path, indexed, reason`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusPath {
    pub path: String,
    pub indexed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbols: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Which file the three repository settings were read from. `project` means this tree's own
/// `<dataDir>/config.json` was read; `global` means it is absent (or unreadable) and the machine
/// file supplied the values. The fallback is normal and silent, so this field is where it becomes
/// visible.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSource {
    Project,
    Global,
}

impl From<RepoConfigSource> for ConfigSource {
    fn from(source: RepoConfigSource) -> Self {
        match source {
            RepoConfigSource::Project => Self::Project,
            RepoConfigSource::Global => Self::Global,
        }
    }
}

/// The repository's identity, as the publish recorded it.
///
/// `branch`, `remote` and `commit` are `null` — never absent — when the store never recorded them: a
/// graph published before this build wrote those keys, or a publish where git could not answer,
/// still answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectBlock {
    pub name: Option<String>,
    pub root: Option<String>,
    pub branch: Option<String>,
    pub remote: Option<String>,
    pub commit: Option<String>,
    pub db_path: String,
}

/// Which file the repository settings came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigBlock {
    pub source: ConfigSource,
}

/// What the served generation holds, and the file it was read from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphBlock {
    pub nodes: i64,
    pub edges: i64,
    pub size_bytes: u64,
    pub db_path: String,
}

/// The `unresolved` census, one key per bucket of the closed taxonomy, always all six.
///
/// Every bucket is present at zero rather than omitted, so a reader never has to tell "no rows" from
/// "this build does not report buckets"; the order is
/// [`sangkar_domain::unresolved::UNRESOLVED_BUCKETS`]'s, which `tests/status.rs` pins.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedBuckets {
    pub builtin_stdlib: u64,
    pub external_package: u64,
    pub internal_module: u64,
    pub dynamic_property_call: u64,
    pub receiver_type_unknown: u64,
    pub ambiguous: u64,
}

impl UnresolvedBuckets {
    /// The six counts of one census, in taxonomy order.
    #[must_use]
    fn of(census: &UnresolvedCensus) -> Self {
        Self {
            builtin_stdlib: count(census.count(UnresolvedBucket::BuiltinStdlib)),
            external_package: count(census.count(UnresolvedBucket::ExternalPackage)),
            internal_module: count(census.count(UnresolvedBucket::InternalModule)),
            dynamic_property_call: count(census.count(UnresolvedBucket::DynamicPropertyCall)),
            receiver_type_unknown: count(census.count(UnresolvedBucket::ReceiverTypeUnknown)),
            ambiguous: count(census.count(UnresolvedBucket::Ambiguous)),
        }
    }
}

/// The flat count with the reason each row never bound. `material` is the part that could still be
/// user-defined code.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedBlock {
    pub total: u64,
    pub material: u64,
    pub buckets: UnresolvedBuckets,
}

/// The indexer's state and the two costs a caller can act on, as
/// [`sangkar_domain::freshness::ToolHealth`] reports them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthBlock {
    pub indexer: Option<String>,
    pub last_publish_ms: u64,
    pub consecutive_failures: i64,
    pub statement_cache_entries: usize,
}

impl From<ToolHealth> for HealthBlock {
    fn from(health: ToolHealth) -> Self {
        Self {
            indexer: health.indexer,
            last_publish_ms: health.last_publish_ms,
            consecutive_failures: health.consecutive_failures,
            statement_cache_entries: health.statement_cache_entries,
        }
    }
}

/// The payload keys `TOOLS-SPEC.md` §2 documents, in that order — serde writes a struct's fields in
/// declaration order, so the declaration *is* the wire contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusPayload {
    pub project: ProjectBlock,
    pub config: ConfigBlock,
    pub graph: GraphBlock,
    pub unresolved: UnresolvedBlock,
    pub languages: Vec<StatusLanguage>,
    pub paths: Vec<StatusPath>,
    pub health: HealthBlock,
}

/// `null` is the payload of the two statuses that carry none: `bootstrapping` (nothing is published
/// yet) and `error` (the budget could not be applied). Both still carry the nine envelope fields.
///
/// The payload is a [`Value`] rather than [`StatusPayload`] because the surface has one envelope
/// type: the registry calls all seven handlers, and a per-tool payload type would make that call a
/// seven-armed cast. The struct above is what builds it, so the key order is still a declaration
/// and not a convention of whoever assembled the object.
pub type StatusEnvelope = Envelope<Value>;

/// Answers "is the graph current and what does it cover" without opening a database, reading the
/// environment or writing to stdout. A call that names no paths asks about the whole repository.
///
/// A reader that cannot answer becomes an `error` envelope with the query's own code
/// (`internal` for a failed statement), not a partial report: a failed read has no truthful
/// payload, and reporting `indexed: 0` would turn a broken read into an absence claim. Its
/// `read_status` says stale with the reason, because no freshness check ran.
pub fn handle(ctx: &ToolContext, args: &StatusArgs) -> StatusEnvelope {
    match answer(ctx, args) {
        Ok(envelope) => envelope,
        Err(error) => read_failure(ctx, error),
    }
}

/// The envelope of a read that failed: the error's own code and message on the unscoped skeleton.
/// It is deliberately not `not_found` — a failed read is not an absence claim.
fn read_failure(ctx: &ToolContext, error: AppError) -> StatusEnvelope {
    fail(
        error,
        EnvelopeSpec::new(
            "status: the answer could not be read".to_owned(),
            ReadStatus {
                generation: String::new(),
                indexed_at: String::new(),
                stale: true,
                stale_reason: Some(UNREAD_ANSWER.to_owned()),
                stale_checked_at: Some((ctx.now)()),
                decisive: None,
            },
            Coverage {
                complete: false,
                indexed: 0,
                skipped: 0,
                failed: 0,
                unresolved_edges: 0,
                not_indexed: Vec::new(),
                tests: None,
            },
        ),
    )
}

/// The body of [`handle`], in the contract's order: read meta, guard the generation, assemble, then
/// sign and measure.
fn answer(ctx: &ToolContext, args: &StatusArgs) -> Result<StatusEnvelope, AppError> {
    let requested = requested_paths(args.paths.as_deref());
    let budget = args.budget();

    let meta = ctx.reader.meta_for()?;
    let latest = ctx.reader.latest_generation()?;
    // A reader with no published generation has nothing to report on: `bootstrapping` is the honest
    // answer, and it is cheaper for the agent than an `ok` envelope with an invented generation id.
    let (Some(generation), Some(generation_row)) = (meta.generation.clone(), latest) else {
        return Ok(unpublished(ctx, &meta));
    };

    // Read once: the identity counts, the census, the staleness manifest and the file ids the
    // unresolved query is scoped to all come from this one pass.
    let files = ctx.reader.all_files()?;
    let file_ids: Vec<i64> = files.iter().map(|file| file.id).collect();
    let census = ctx.reader.census_by_language()?;

    // Freshness is computed once, from the reader: both the envelope's `read_status` and the summary
    // are worded from it. A tool decides what to show; the domain module decides what is true. The
    // verdict's clock is `FreshnessOptions`' own epoch milliseconds — `ctx.now` speaks the wire's
    // ISO-8601 string, and no public parser bridges the two, so the injected clock dates the
    // *bootstrap* answer (which this module builds itself) and not this one.
    let freshness = check_freshness(
        ctx.reader.as_ref(),
        &FreshnessOptions {
            now_ms: None,
            repo_root: Some(ctx.config.repo_root.as_path()),
        },
    );
    let stale_files = files
        .iter()
        .filter(|file| file.indexed_generation != generation)
        .count();

    // One scope, one assembly: `paths: None` is the whole repository (the no-path request), and the
    // file ids pin the unresolved query to the repo-wide row set even for a scoped request — the
    // same measurement `expand` and `summary` publish as `unresolved_edges`.
    let scope = CoverageScope {
        paths: (!requested.is_empty()).then(|| requested.clone()),
        generation: generation.clone(),
        stale: freshness.stale,
        file_ids: Some(file_ids),
        include_tests: true,
    };
    let assembly = assemble(&coverage_source(ctx.reader.as_ref()), &scope)?;

    let failed = files.iter().filter(|file| is_failed_parse(file)).count();
    let coverage = Coverage {
        // The repository's verdict: one failed parse anywhere means an absence claim over this graph
        // is a lower bound, whatever slice of it the request named.
        complete: !files.is_empty() && assembly.coverage.skipped == 0 && failed == 0,
        indexed: count(files.len()),
        skipped: assembly.coverage.skipped,
        failed: count(failed),
        unresolved_edges: count(assembly.unresolved.len()),
        not_indexed: assembly.coverage.not_indexed.clone(),
        tests: assembly.coverage.tests.clone(),
    };

    let read_status = read_status_of(
        &freshness,
        &generation,
        &generation_row,
        stale_files,
        files.len(),
    );
    // The requested paths at file granularity are the payload's own query; `assemble` already ran it
    // for a scoped request, and running it again here is a statement-cache hit rather than a second
    // population — `path_report` needs the rows themselves, which the assembly does not hand back.
    let rows = ctx.reader.rows_for_paths(&requested)?;
    let report = path_report(ctx, &requested, &rows)?;
    let db_path = ctx.config.data_dir.join("graph.db").display().to_string();
    let unresolved_census = bucket_census(&assembly.unresolved);

    let path_count = count(report.entries.len());
    let unresolved_total = count(assembly.unresolved.len());
    let payload = StatusPayload {
        project: ProjectBlock {
            name: meta.project_name.clone(),
            root: meta.project_root.clone(),
            branch: meta.project_branch.clone(),
            remote: meta.project_remote.clone(),
            commit: generation_row.base_commit.clone(),
            db_path: db_path.clone(),
        },
        config: ConfigBlock {
            source: ctx.config.repo_config_source.into(),
        },
        graph: GraphBlock {
            nodes: generation_row.nodes,
            edges: generation_row.edges,
            size_bytes: ctx.db_size_bytes,
            db_path,
        },
        unresolved: UnresolvedBlock {
            total: unresolved_total,
            material: count(unresolved_census.material),
            buckets: UnresolvedBuckets::of(&unresolved_census),
        },
        languages: language_entries(census),
        paths: report.entries,
        health: health(ctx.reader.as_ref()).into(),
    };

    let totals = Totals::new(relation_for(&read_status, &coverage))
        .with_total("paths", Some(path_count))
        .with_total("not_indexed", Some(assembly.not_indexed_total));
    let spec = EnvelopeSpec::new(
        summary_of(
            &coverage,
            &generation,
            stale_files,
            freshness.files_changed_since,
            requested.len(),
            report.indexed_paths,
            assembly.not_indexed_total,
        ),
        read_status,
        coverage,
    )
    // The routing table, not this handler's guess: `route()` republishes the same list, so the
    // envelope a unit test sees is the envelope the surface serves.
    .with_totals(totals)
    .with_next_tools(next_tools_for(NAME));

    let args_hash = args_hash_of(&requested, budget);
    // Serialising the struct is what publishes the payload's key order; a `Value` cannot fail here
    // (no non-string keys, no non-finite numbers), so an error is a contract bug, not caller input.
    let payload = serde_json::to_value(&payload).map_err(|error| {
        AppError::internal(format!("the status payload is not serialisable: {error}"))
    })?;
    let envelope: StatusEnvelope = ok(payload, spec.clone());
    let emitted = match emit(envelope, budget, |position| {
        ctx.cursor.sign(&CursorPayload {
            generation: generation.clone(),
            tool: NAME.to_owned(),
            args_hash: args_hash.clone(),
            position: u64::try_from(position).unwrap_or(u64::MAX),
        })
    }) {
        Ok(emitted) => emitted,
        // A budget above the ceiling is a client mistake and part of the closed error union, so it
        // comes back as an envelope rather than as a thrown handler. Anything else here is a genuine
        // bug, and the same branch keeps it in the contract's shape with no partial payload.
        Err(error) => return Ok(fail(error, refused(spec))),
    };

    // A budget below the floor this report needs cannot be cut to fit, so `emit` hands the envelope
    // back untouched: serving it would be a report many times the allowance. The tool answers with
    // the floor instead, and the same envelope-shape rule as the ceiling refuses it.
    let floor = budget_floor_of(&emitted);
    if floor > budget {
        return Ok(fail(
            AppError::new(
                ErrorCode::BUDGET_TOO_LARGE,
                format!("budget {budget} is below the floor this report needs"),
            )
            .with_hint(format!(
                "raise the budget to at least {floor} tokens; the default is {STATUS_DEFAULT_TOKENS}"
            )),
            refused(spec),
        ));
    }
    Ok(emitted)
}

/// The `argsHash` a cursor minted by this tool is pinned to: `sha256:` plus the hex digest of the
/// compact JSON of `{budget, paths}` (`handler.ts:555-560`).
///
/// Public because it is wire contract, not an internal detail: the cursor a truncated answer mints
/// carries this exact string, and `tests/status.rs` pins it against the digests inside the captured
/// goldens' own tokens — the one place the TypeScript's answer is recorded byte for byte.
#[must_use]
pub fn args_hash_of(requested: &[String], budget: u64) -> String {
    // `JSON.stringify({ budget, paths })`: two keys, in that order, no whitespace. A `Map` with
    // `preserve_order` writes insertion order, which is the whole reason the feature is enabled.
    let mut canonical = serde_json::Map::new();
    canonical.insert("budget".to_owned(), serde_json::Value::from(budget));
    canonical.insert(
        "paths".to_owned(),
        serde_json::Value::Array(
            requested
                .iter()
                .map(|path| serde_json::Value::String(path.clone()))
                .collect(),
        ),
    );
    let text = serde_json::Value::Object(canonical).to_string();
    let mut hex = String::with_capacity(64);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in Sha256::digest(text.as_bytes()) {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    format!("sha256:{hex}")
}

/// The spec a refused call carries: the answer's shape, with a summary that says it was not served.
fn refused(mut spec: EnvelopeSpec) -> EnvelopeSpec {
    spec.summary = REFUSED_SUMMARY.to_owned();
    spec
}

/// `bootstrapping` with the fields an unpublished database has: no generation and no coverage.
fn unpublished(ctx: &ToolContext, meta: &GraphMeta) -> StatusEnvelope {
    bootstrapping(
        BootstrappingSpec::new(
            EnvelopeSpec::new(
                UNPUBLISHED_SUMMARY,
                ReadStatus {
                    // The empty id is the truthful value: a fabricated one would give a cursor
                    // something to bind to that no publish will ever match.
                    generation: meta.generation.clone().unwrap_or_default(),
                    indexed_at: String::new(),
                    stale: true,
                    stale_reason: Some(UNREADABLE_GENERATION.to_owned()),
                    stale_checked_at: Some((ctx.now)()),
                    decisive: None,
                },
                Coverage {
                    complete: false,
                    indexed: 0,
                    skipped: 0,
                    failed: 0,
                    unresolved_edges: 0,
                    not_indexed: Vec::new(),
                    tests: None,
                },
            )
            .with_next_tools(["status"]),
            STATUS_RETRY_AFTER_MS,
        )
        .with_message(UNPUBLISHED_MESSAGE)
        .with_hint(UNPUBLISHED_HINT),
    )
}

/// The freshness verdict as the envelope spells it. `stale` and `stale_checked_at` come from
/// [`sangkar_domain::freshness`] untouched; this function only chooses the words.
///
/// The graph-side fact stays visible, because it is a different fact from disk drift: a `file` row
/// the indexer wrote in an earlier generation means the served graph is behind its own tree. It is
/// reported alone when freshness found nothing, and appended when freshness did find something, so
/// one reason never hides the other. A fresh answer carries no `stale_reason` key at all.
///
/// `indexed_at` is the served publish's `published_at`: `sangkar_store`'s `GraphMeta` carries no
/// timestamp, and the newest publish is the record of when the bytes being described were written.
fn read_status_of(
    freshness: &Freshness,
    generation: &str,
    generation_row: &GenerationRow,
    stale_files: usize,
    indexed_files: usize,
) -> ReadStatus {
    let mut status = ReadStatus {
        generation: generation.to_owned(),
        indexed_at: generation_row.published_at.clone(),
        stale: freshness.stale,
        stale_reason: None,
        stale_checked_at: freshness.stale_checked_at.clone(),
        decisive: None,
    };
    let older = older_generation_reason(stale_files, indexed_files);
    if let Some(reason) = freshness_reason(freshness, indexed_files) {
        status.stale = true;
        status.stale_reason = Some(match older {
            Some(older) => format!("{reason}; {older}"),
            None => reason,
        });
        return status;
    }
    if let Some(older) = older {
        status.stale = true;
        status.stale_reason = Some(older);
    }
    status
}

/// The words for each filesystem-side verdict; the graph-side fact is handled beside this.
fn freshness_reason(freshness: &Freshness, indexed_files: usize) -> Option<String> {
    match freshness.stale_reason.as_deref()? {
        // The one verdict that makes every other comparison meaningless: the graph describes another
        // directory, so no count of this one belongs next to it.
        "root_mismatch" => Some("the graph was built for a different project_root".to_owned()),
        "base_commit_moved" => Some("the indexed base commit is no longer HEAD".to_owned()),
        "watcher_stalled" => {
            Some("the indexer has been busy for over 10 minutes with nothing published".to_owned())
        }
        "files_changed" => Some(format!(
            "{} of {indexed_files} indexed files changed on disk since the index",
            freshness.files_changed_since
        )),
        // `none` is a value in the TypeScript and `None` here; any other string is a reason this
        // port does not have words for, and inventing them would put an unmeasured claim in a
        // `stale_reason`.
        _ => None,
    }
}

/// The graph-side half: rows the indexer wrote in a generation other than the one being served.
fn older_generation_reason(stale_files: usize, indexed_files: usize) -> Option<String> {
    (stale_files > 0).then(|| {
        format!(
            "{stale_files} of {indexed_files} indexed files were written by an earlier generation"
        )
    })
}

/// The census with the one note the spec asks for.
///
/// A language that produced no parse at all is exactly the case CRG's census hid by listing `xslt`
/// and `liquid` as supported, so `parsed: 0` and a note together say "this language is unavailable
/// to the build" instead of implying zero files of that language exist. The unattributed bucket
/// (`language: null`) gets no note: the note names a language, and that bucket has no name to
/// attach it to.
fn language_entries(census: Vec<LanguageCensus>) -> Vec<StatusLanguage> {
    census
        .into_iter()
        .map(|row| {
            let note = (row.language.is_some() && row.parsed == 0 && row.failed > 0)
                .then(|| NOTE_GRAMMAR_UNAVAILABLE.to_owned());
            StatusLanguage {
                language: row.language,
                files: row.files,
                parsed: row.parsed,
                failed: row.failed,
                note,
            }
        })
        .collect()
}

/// One call's path entries and how many of the requested paths matched an indexed file.
struct PathReport {
    entries: Vec<StatusPath>,
    indexed_paths: usize,
}

/// Turns the requested paths into payload entries.
///
/// A requested path is answered at the granularity it asks for: naming a file returns that file with
/// its symbol count and digest, while naming a directory returns the files and skipped paths under
/// it, each with its own reason. That expansion is what makes "is `src/auth/` indexed, and did it
/// parse cleanly?" one call, which is the reason `paths` exists at all (`TOOLS-SPEC.md` §2).
///
/// Overlapping requests (`["src", "src/a.ts"]`) emit each path once: the first request that matches
/// a row owns it, so a payload entry is never duplicated by the shape of the request.
fn path_report(
    ctx: &ToolContext,
    requested: &[String],
    rows: &PathCoverage,
) -> Result<PathReport, AppError> {
    let mut entries: Vec<StatusPath> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut indexed_paths = 0_usize;

    for path in requested {
        let exact = rows.files.iter().find(|file| &file.path == path);
        if let Some(file) = exact {
            indexed_paths += 1;
            let symbols = ctx.reader.nodes_by_file(file.id)?.len();
            push_unique(&mut entries, &mut seen, entry_for_file(file, Some(symbols)));
            continue;
        }

        let under: Vec<&IndexedFileRow> = rows
            .files
            .iter()
            .filter(|file| is_under_prefix(&file.path, path))
            .collect();
        let skipped: Vec<&SkippedPathRow> = rows
            .not_indexed
            .iter()
            .filter(|row| is_under_prefix(&row.path, path))
            .collect();
        if !under.is_empty() {
            indexed_paths += 1;
        }
        let unknown = under.is_empty() && skipped.is_empty();
        for file in under {
            push_unique(&mut entries, &mut seen, entry_for_file(file, None));
        }
        for row in skipped {
            push_unique(
                &mut entries,
                &mut seen,
                StatusPath {
                    path: row.path.clone(),
                    indexed: false,
                    sha256: None,
                    symbols: None,
                    reason: Some(row.reason.clone()),
                },
            );
        }
        if unknown {
            // Neither `file` nor `not_indexed` knows this path, so the indexer never looked at it.
            // That is a different fact from "looked at and skipped", and it is the one that must not
            // read as zero.
            push_unique(
                &mut entries,
                &mut seen,
                StatusPath {
                    path: path.clone(),
                    indexed: false,
                    sha256: None,
                    symbols: None,
                    reason: Some(REASON_NOT_INDEXED_UNKNOWN.to_owned()),
                },
            );
        }
    }

    Ok(PathReport {
        entries,
        indexed_paths,
    })
}

/// Appends an entry the first time its path is seen.
fn push_unique(entries: &mut Vec<StatusPath>, seen: &mut HashSet<String>, entry: StatusPath) {
    if seen.insert(entry.path.clone()) {
        entries.push(entry);
    }
}

/// One indexed file's entry; `symbols` is passed only when the request justified the query.
fn entry_for_file(file: &IndexedFileRow, symbols: Option<usize>) -> StatusPath {
    StatusPath {
        path: file.path.clone(),
        indexed: true,
        sha256: Some(file.sha256.clone()),
        symbols,
        reason: None,
    }
}

/// The requested paths in the one form the prefix queries and the payload agree on: repo-relative,
/// forward slashes, no duplicates, request order preserved.
///
/// The input is agent-supplied, so an empty path is dropped instead of reaching a prefix query that
/// would match the whole graph. The TypeScript also drops a non-string element; the argument type
/// here is `String`, so that case is a compile error rather than a runtime filter.
fn requested_paths(raw: Option<&[String]>) -> Vec<String> {
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for candidate in raw.unwrap_or(&[]) {
        let path = normalize_rel(candidate);
        if path.is_empty() || !seen.insert(path.clone()) {
            continue;
        }
        unique.push(path);
    }
    unique
}

/// The skipped clause of the summary line: the count that matches the scope, and the repository's
/// total only when it says more than the scope did — otherwise the line would name one number under
/// the other's label.
fn skipped_clause(skipped: u64, not_indexed_total: u64) -> Option<String> {
    if skipped == 0 {
        return None;
    }
    Some(if not_indexed_total > skipped {
        format!("{skipped} skipped of {not_indexed_total} repo-wide")
    } else {
        format!("{skipped} skipped")
    })
}

/// The one-line answer every envelope carries: the counts, then the scope they were counted over.
fn summary_of(
    coverage: &Coverage,
    generation: &str,
    stale_files: usize,
    files_changed: usize,
    requested_count: usize,
    indexed_paths: usize,
    not_indexed_total: u64,
) -> String {
    let mut clauses = vec![
        format!("{} files indexed", coverage.indexed),
        format!("generation {generation}"),
    ];
    clauses.extend(skipped_clause(coverage.skipped, not_indexed_total));
    if coverage.failed > 0 {
        clauses.push(format!("{} failed to parse", coverage.failed));
    }
    if stale_files > 0 {
        clauses.push(format!("{stale_files} written by an earlier generation"));
    }
    if files_changed > 0 {
        clauses.push(format!("{files_changed} changed on disk since the index"));
    }
    let scope = if requested_count == 0 {
        "Whole repository.".to_owned()
    } else {
        format!("{indexed_paths} of {requested_count} requested paths indexed.")
    };
    format!("{}. {scope}", clauses.join(", "))
}

/// `partial` and `failed` parses are both "we did not fully read this file" for coverage
/// (`DESIGN.md` §9.2), but `status` reports only the outright failures: a partial parse produced
/// nodes, and the file's `parse_error_ranges` are the place that fact belongs.
fn is_failed_parse(file: &IndexedFileRow) -> bool {
    file.parse_status == "failed"
}

/// A `usize` wire count as the `u64` the payload publishes. A count wider than a `u64` cannot exist
/// in a vector, so the saturation arm is unreachable rather than lossy in practice.
fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
