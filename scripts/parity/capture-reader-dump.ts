#!/usr/bin/env bun
/**
 * Dumps what the TypeScript `GraphReader` answers for one database into
 * `rust/tests/golden/reader/<fixture>.json`, so
 * `rust/crates/sangkar-store/tests/reader_golden.rs` can prove the Rust reader answers the same
 * rows from the same file — method by method, before a handler depends on it.
 *
 * One key per reader method (the 22 of `src/shared/store/read/reader.ts`), plus the arguments the
 * projection was built from (`target.*`) so the Rust side never has to re-derive them. Every value
 * is the same *canonical text* both sides produce: compact JSON, declared key order, `null`
 * fields omitted, lists in the reader's own order. The writer is hand-rolled in both languages on
 * purpose — `sangkar-store` has no `serde_json`, so the comparable form is a string both sides can
 * emit identically, and the Rust test diffs it structurally (key by key, element by element).
 *
 * Two facts shape the projection, and neither is a relaxation:
 *
 * - **Generation ids are random per publish** (`g-xxxxxx`, `src/indexer/worker.ts`), so they are
 *   projected as `@current` (the id `latest_generation()` reports) or `@previous`. Carrying the
 *   literal id would make the committed golden differ on every capture. Every other id is a
 *   rowid and is compared literally.
 * - **Timestamps are excluded** (`generation.published_at`, `duration_ms`), as are the fields a
 *   machine, not the reader, decides: `meta.project_root` (a checkout path), `project_branch` and
 *   `project_remote` (git's answer about wherever the archive lives). The archive is copied to a
 *   temp directory outside git, so those three are absent and `base_commit`/`worktree_dirty` are
 *   the stable "no repository" answer.
 *
 * The databases are *not* committed. Each fixture is archived to a temp directory, indexed twice
 * (the second publish follows deleting one file, so `removed_symbols_for` has real tombstones
 * instead of comparing empty to empty), and the graph lands in
 * `target/parity-reader-db/<fixture>/graph.db`. The run prints the exact command that compares
 * that file against the golden, one line per fixture.
 *
 * Usage, from the repository root: `~/.bun/bin/bun run scripts/parity/capture-reader-dump.ts`
 * Set `SANGKAR_PARITY_READER_KEEP=1` to leave the temp archives in place.
 */
import { spawnSync } from "node:child_process";
import { cp, mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { type Conn, openReader } from "#shared/store/db";
import {
  type GraphReader,
  type IndexedFileRow,
  type NodeMetrics,
  type NodeRow,
  makeReader,
} from "#shared/store/read/reader";

const REPO_ROOT = resolve(import.meta.dir, "..", "..");
const CLI = join(REPO_ROOT, "bin", "sangkar.ts");
const FIXTURE_ROOT = join(REPO_ROOT, "rust", "tests", "parity", "fixtures");
const GOLDEN_DIR = join(REPO_ROOT, "rust", "tests", "golden", "reader");
/** The databases, under `target/` so a capture never lands in git and never leaves the tree. */
const DB_ROOT = join(REPO_ROOT, "target", "parity-reader-db");
/** The archives: outside git, so the publish's git-derived meta fields are the stable "none". */
const ARCHIVE_ROOT = join(tmpdir(), "sangkar-parity-reader");

interface FixturePlan {
  readonly name: "polyglot" | "broken";
  /**
   * Deleted between the two publishes. The symbol and file nodes it leaves behind are what
   * `removed_symbols_for` reads back as tombstones, so that method compares real rows.
   */
  readonly removedFile: string;
}

const FIXTURES: readonly FixturePlan[] = [
  // `duplicate.ts` is not the file `handle` lives in, so the duplicate-name projection keeps its
  // four-row tie after the deletion.
  { name: "polyglot", removedFile: "src/duplicate.ts" },
  { name: "broken", removedFile: "src/entry.ts" },
];

/** The prefixes `rows_for_paths` is asked about: `src` has rows, `lib` matches nothing. */
const PREFIXES = ["src", "lib"] as const;

/** A limit wide enough to rank, narrow enough that the golden stays readable. */
const RANK_LIMIT = 5;

// ---------------------------------------------------------------------------------------------
// The canonical writer. Both languages implement exactly these five shapes.
// ---------------------------------------------------------------------------------------------

/** A JSON string with the escapes both writers emit: quote, backslash, the five short controls. */
function quote(text: string): string {
  let out = '"';
  for (const ch of text) {
    switch (ch) {
      case '"':
        out += '\\"';
        break;
      case "\\":
        out += "\\\\";
        break;
      case "\n":
        out += "\\n";
        break;
      case "\r":
        out += "\\r";
        break;
      case "\t":
        out += "\\t";
        break;
      case "\b":
        out += "\\b";
        break;
      case "\f":
        out += "\\f";
        break;
      default: {
        const code = ch.codePointAt(0) ?? 0;
        out += code < 0x20 ? `\\u${code.toString(16).padStart(4, "0")}` : ch;
      }
    }
  }
  return `${out}"`;
}

function str(text: string): string {
  return quote(text);
}

function int(value: number): string {
  if (!Number.isSafeInteger(value)) {
    throw new Error(`${value} is not an integer, so it has no canonical text`);
  }
  return String(value);
}

function bool(value: boolean): string {
  return value ? "true" : "false";
}

/** `[a,b]` — no spaces, so the text is a structural rendering rather than a pretty one. */
function arr(items: readonly string[]): string {
  return `[${items.join(",")}]`;
}

/** `{"k":v,…}` in the declared order; a `null`/`undefined` value drops the field entirely. */
function obj(parts: ReadonlyArray<readonly [string, string | null | undefined]>): string {
  const kept: string[] = [];
  for (const [key, value] of parts) {
    if (value !== null && value !== undefined) {
      kept.push(`${quote(key)}:${value}`);
    }
  }
  return `{${kept.join(",")}}`;
}

function opt<T>(value: T | null | undefined, render: (input: T) => string): string | null {
  return value === null || value === undefined ? null : render(value);
}

/**
 * An absent *row* renders as JSON `null`, which is a value both sides can compare; only a `null`
 * field is dropped (see {@link obj}), because a field has no third state and a row does.
 */
function nullable<T>(value: T | null | undefined, render: (input: T) => string): string {
  return value === null || value === undefined ? "null" : render(value);
}

/**
 * The exact `f64` the database returned, as its IEEE-754 bit pattern in decimal. `bm25()` returns
 * a real, and the two builds may not agree on the last bit of a decimal reformat — but they do
 * agree on the bits when they agree on the arithmetic, and a string is the same text in both
 * languages. `score.toFixed(6)` would round differently in JS and Rust on an exact tie.
 */
function scoreBits(score: number): string {
  const view = new DataView(new ArrayBuffer(8));
  view.setFloat64(0, score, false);
  return view.getBigUint64(0, false).toString();
}

/**
 * A generation id as a stable label, already quoted for the canonical writer. The id is random per
 * publish, so the golden can only record its *relation* to the newest publish — which is the fact
 * every row carrying one asserts.
 */
function generationText(id: string | null | undefined, current: string | null): string | null {
  if (id === null || id === undefined) {
    return null;
  }
  return str(id === current ? "@current" : "@previous");
}

// ---------------------------------------------------------------------------------------------
// One projection per row type, in the order both readers declare the fields.
// ---------------------------------------------------------------------------------------------

function nodeText(node: NodeRow): string {
  return obj([
    ["id", int(node.id)],
    ["file_id", int(node.fileId)],
    ["path", str(node.path)],
    ["parent_id", opt(node.parentId, int)],
    ["kind", str(node.kind)],
    ["subkind", opt(node.subkind, str)],
    ["name", str(node.name)],
    ["qn", str(node.qn)],
    ["start_line", int(node.startLine)],
    ["end_line", int(node.endLine)],
    ["signature", opt(node.signature, str)],
    ["docstring", opt(node.docstring, str)],
    ["is_test", bool(node.isTest)],
    // The Rust row keeps the raw column, so the projection carries the same text: `JSON.stringify`
    // of the parsed props is the stored text again, which the guard in `propsRoundTrip` proves.
    ["props", JSON.stringify(node.props)],
  ]);
}

function fileText(file: IndexedFileRow, current: string | null): string {
  return obj([
    ["id", int(file.id)],
    ["path", str(file.path)],
    ["language", str(file.language)],
    ["sha256", str(file.sha256)],
    ["size", int(file.size)],
    ["line_count", int(file.lineCount)],
    ["parse_status", str(file.parseStatus)],
    ["parse_error_ranges", opt(file.parseErrorRanges, str)],
    ["indexed_generation", generationText(file.indexedGeneration, current)],
  ]);
}

function metricsText(metrics: NodeMetrics): string {
  return obj([
    ["node_id", int(metrics.nodeId)],
    ["in_degree", int(metrics.inDegree)],
    ["out_degree", int(metrics.outDegree)],
    ["is_entry", bool(metrics.isEntry)],
    ["tested_depth", opt(metrics.testedDepth, int)],
    ["reachable_count", opt(metrics.reachableCount, int)],
  ]);
}

// ---------------------------------------------------------------------------------------------
// The targets: arguments derived from the database itself, recorded in the golden.
// ---------------------------------------------------------------------------------------------

interface Targets {
  readonly duplicateName: string;
  readonly qn: string;
  readonly membersParentId: number;
  readonly fileId: number;
  readonly nodeId: number;
  readonly filePath: string;
  readonly unresolvedFileIds: readonly number[];
  readonly testGapIds: readonly number[];
  readonly ftsQuery: string;
}

interface TargetSqlRow {
  readonly value: string | number | null;
}

/** One scalar from one statement; a statement returning no row is a fixture the plan cannot use. */
function scalar(conn: Conn, label: string, sql: string): string {
  const row = conn.sqlite.query<TargetSqlRow, []>(sql).get();
  const value = row?.value ?? null;
  if (value === null) {
    throw new Error(`${label}: the fixture answered nothing for ${sql}`);
  }
  return String(value);
}

function idList(conn: Conn, sql: string): number[] {
  return conn.sqlite
    .query<{ readonly id: number }, []>(sql)
    .all()
    .map((row) => row.id);
}

/**
 * The arguments of the 22 projections, derived from the database rather than hardcoded, so a
 * fixture edit cannot leave a projection pointing at a row that no longer exists. The values are
 * written into the golden (`target.*`) and read back by the Rust test, which is the point: one
 * side decides the arguments, the other replays them.
 */
function deriveTargets(conn: Conn, reader: GraphReader): Targets {
  // The most-duplicated name: the widest tie `find_by_name` can be asked to order.
  const duplicateName = scalar(
    conn,
    "duplicate name",
    "SELECT n.name AS value FROM node n GROUP BY n.name ORDER BY count(*) DESC, n.name ASC LIMIT 1",
  );

  const qn = scalar(
    conn,
    "qn",
    "SELECT n.qn AS value FROM node n WHERE n.parent_id IS NOT NULL ORDER BY n.id ASC LIMIT 1",
  );

  // A class when the fixture has one, else the parent with the most children.
  const membersParentId = Number(
    scalar(
      conn,
      "members parent",
      "SELECT p.id AS value FROM node p JOIN node c ON c.parent_id = p.id" +
        " GROUP BY p.id ORDER BY (p.kind = 'class') DESC, count(*) DESC, p.id ASC LIMIT 1",
    ),
  );

  // The file that defines the most nodes, so `nodes_by_file` returns a full source-ordered list.
  const fileId = Number(
    scalar(
      conn,
      "file id",
      "SELECT n.file_id AS value FROM node n GROUP BY n.file_id" +
        " ORDER BY count(*) DESC, n.file_id ASC LIMIT 1",
    ),
  );

  // The busiest node: at least three incident edges on either fixture, which is the contract's
  // "a node with at least three edges".
  const nodeId = Number(
    scalar(
      conn,
      "edge node",
      "SELECT n.id AS value," +
        " (SELECT count(*) FROM edge e WHERE e.dst_id = n.id)" +
        " + (SELECT count(*) FROM edge e WHERE e.src_id = n.id) AS degree" +
        " FROM node n ORDER BY degree DESC, n.id ASC LIMIT 1",
    ),
  );

  const filePath = scalar(
    conn,
    "file path",
    "SELECT f.path AS value FROM file f ORDER BY f.id ASC LIMIT 1",
  );

  const unresolvedFileIds = idList(
    conn,
    "SELECT DISTINCT u.file_id AS id FROM unresolved u ORDER BY u.file_id ASC LIMIT 3",
  );

  // Two ids the gap test keeps and one it drops, when the fixture has both: the projection then
  // covers the filter as well as the ordering. A fixture with only gaps is topped up.
  const gapIds = idList(
    conn,
    "SELECT m.node_id AS id FROM node_metrics m" +
      " WHERE m.tested_depth IS NULL OR m.tested_depth > 2" +
      " ORDER BY (m.tested_depth IS NULL) DESC, m.tested_depth DESC, m.node_id ASC LIMIT 2",
  );
  const nonGapIds = idList(
    conn,
    "SELECT m.node_id AS id FROM node_metrics m" +
      " WHERE m.tested_depth IS NOT NULL AND m.tested_depth <= 2" +
      " ORDER BY m.tested_depth ASC, m.node_id ASC LIMIT 1",
  );
  const chosen = [...gapIds, ...nonGapIds];
  const testGapIds =
    chosen.length >= 3
      ? chosen.slice(0, 3)
      : [
          ...chosen,
          ...idList(
            conn,
            "SELECT m.node_id AS id FROM node_metrics m WHERE m.node_id NOT IN" +
              ` (${chosen.length === 0 ? "NULL" : chosen.join(", ")})` +
              " ORDER BY m.node_id ASC LIMIT " +
              String(3 - chosen.length),
          ),
        ];

  // A query that actually matches: the first symbol name, in id order, whose `MATCH` answers.
  let ftsQuery: string | null = null;
  const candidates = conn.sqlite
    .query<{ readonly name: string }, []>(
      "SELECT n.name AS name FROM node n WHERE n.kind <> 'file' ORDER BY n.id ASC LIMIT 50",
    )
    .all();
  for (const candidate of candidates) {
    try {
      if (reader.readNodeFts(candidate.name, RANK_LIMIT).length > 0) {
        ftsQuery = candidate.name;
        break;
      }
    } catch {
      // A name `MATCH` cannot parse as a query is not a candidate; the next one may be.
    }
  }
  if (ftsQuery === null) {
    throw new Error("read_node_fts: no symbol name in the graph matches a MATCH query");
  }

  return {
    duplicateName,
    qn,
    membersParentId,
    fileId,
    nodeId,
    filePath,
    unresolvedFileIds,
    testGapIds,
    ftsQuery,
  };
}

// ---------------------------------------------------------------------------------------------
// The projections, one per method.
// ---------------------------------------------------------------------------------------------

function renderTargets(targets: Targets): Array<[string, string]> {
  return [
    ["target.duplicate_name", str(targets.duplicateName)],
    ["target.qn", str(targets.qn)],
    ["target.members_parent_id", int(targets.membersParentId)],
    ["target.file_id", int(targets.fileId)],
    ["target.node_id", int(targets.nodeId)],
    ["target.file_path", str(targets.filePath)],
    ["target.unresolved_file_ids", arr(targets.unresolvedFileIds.map(int))],
    ["target.test_gap_ids", arr(targets.testGapIds.map(int))],
    ["target.fts_query", str(targets.ftsQuery)],
    // `@current` is the only label a caller can resolve; the Rust test maps it to the id its own
    // `latest_generation()` reports, so no random id is written down.
    ["target.removed_generation", str("@current")],
  ];
}

/** Every method the golden covers, in the order the contract lists them. */
const METHODS = [
  "find_by_name",
  "find_by_qn",
  "members",
  "nodes_by_file",
  "inbound",
  "outbound",
  "metrics_for",
  "rows_for_paths",
  "not_indexed_all",
  "not_indexed_count",
  "unresolved_for_files",
  "file_by_path",
  "all_files",
  "census_by_language",
  "tests_census",
  "meta_for",
  "latest_generation",
  "removed_symbols_for",
  "entry_points",
  "hotspots",
  "test_gaps",
  "read_node_fts",
] as const;

function project(reader: GraphReader, targets: Targets): Array<[string, string]> {
  const latest = reader.latestGeneration();
  const current = latest?.id ?? null;

  const coverage = (prefixes: readonly string[]): string => {
    const answer = reader.rowsForPaths(prefixes);
    return obj([
      ["files", arr(answer.indexed.map((file) => fileText(file, current)))],
      ["not_indexed", arr(answer.notIndexed.map(skippedText))],
      ["indexed", int(answer.indexed.length)],
      ["skipped", int(answer.notIndexed.length)],
    ]);
  };

  const meta = reader.metaFor();
  const tests = reader.testsCensus();

  const values: Array<[string, string]> = [
    [
      "find_by_name",
      arr(reader.findByName(targets.duplicateName).map(nodeText)),
    ],
    [
      "find_by_qn",
      obj([
        ["hit", nullable(reader.findByQn(targets.qn), nodeText)],
        // A `qn` no publish can contain: the miss is the `null` half of the method's contract.
        ["miss", nullable(reader.findByQn("no/such/qn.ts::__absent__"), nodeText)],
      ]),
    ],
    ["members", arr(reader.members(targets.membersParentId).map(nodeText))],
    ["nodes_by_file", arr(reader.nodesByFile(targets.fileId).map(nodeText))],
    ["inbound", arr(reader.inbound(targets.nodeId).map(neighbourText))],
    ["outbound", arr(reader.outbound(targets.nodeId).map(neighbourText))],
    [
      "metrics_for",
      obj([
        ["hit", nullable(reader.metricsFor(targets.nodeId), metricsText)],
        // `-1` is not a rowid any publish mints, so the unmeasured answer is compared too.
        ["miss", nullable(reader.metricsFor(-1), metricsText)],
      ]),
    ],
    ["rows_for_paths", obj(PREFIXES.map((prefix) => [prefix, coverage([prefix])] as const))],
    ["not_indexed_all", arr(reader.notIndexedAll().map(skippedText))],
    ["not_indexed_count", int(reader.notIndexedCount())],
    [
      "unresolved_for_files",
      arr(reader.unresolvedForFiles([...targets.unresolvedFileIds]).map(unresolvedText)),
    ],
    [
      "file_by_path",
      obj([
        ["hit", nullable(reader.fileByPath(targets.filePath), (file) => fileText(file, current))],
        ["miss", nullable(reader.fileByPath("no/such/path.ts"), (file) => fileText(file, current))],
      ]),
    ],
    ["all_files", arr(reader.allFiles().map((file) => fileText(file, current)))],
    [
      "census_by_language",
      arr(
        reader
          .censusByLanguage()
          .map((row) =>
            obj([
              ["language", opt(row.language, str)],
              ["files", int(row.files)],
              ["parsed", int(row.parsed)],
              ["failed", int(row.failed)],
            ]),
          ),
      ),
    ],
    [
      "tests_census",
      obj([
        ["nodes", int(tests.nodes)],
        [
          "languages",
          arr(
            tests.languages.map((entry) =>
              obj([
                ["language", str(entry.language)],
                ["nodes", int(entry.nodes)],
              ]),
            ),
          ),
        ],
      ]),
    ],
    [
      "meta_for",
      obj([
        ["project_name", opt(meta.projectName, str)],
        ["generation", generationText(meta.generation, current)],
        ["indexer_state", opt(meta.indexerState, str)],
        ["consecutive_failures", int(meta.consecutiveFailures)],
      ]),
    ],
    [
      "latest_generation",
      nullable(latest, (row) =>
        obj([
          ["seq", int(row.seq)],
          ["worktree_dirty", bool(row.worktreeDirty)],
          ["files_indexed", int(row.filesIndexed)],
          ["nodes", int(row.nodes)],
          ["edges", int(row.edges)],
          ["mode", str(row.mode)],
          ["status", str(row.status)],
        ]),
      ),
    ],
    [
      "removed_symbols_for",
      current === null
        ? arr([])
        : arr(
            reader.removedSymbolsFor(current).map((row) =>
              obj([
                ["qn", str(row.qn)],
                ["kind", str(row.kind)],
                ["file_id", opt(row.fileId, int)],
                ["last_seen_generation", generationText(row.lastSeenGeneration, current)],
                ["removed_in_generation", generationText(row.removedInGeneration, current)],
                ["had_callers", bool(row.hadCallers)],
              ]),
            ),
          ),
    ],
    [
      "entry_points",
      arr(
        reader.entryPoints(RANK_LIMIT).map((row) =>
          obj([
            ["id", int(row.id)],
            ["qn", str(row.qn)],
            ["kind", str(row.kind)],
            ["file", str(row.file)],
            ["lines", arr([int(row.lines[0]), int(row.lines[1])])],
            ["reachable", opt(row.reachable, int)],
          ]),
        ),
      ),
    ],
    [
      "hotspots",
      arr(
        reader.hotspots(RANK_LIMIT).map((row) =>
          obj([
            ["id", int(row.id)],
            ["qn", str(row.qn)],
            ["kind", str(row.kind)],
            ["file", str(row.file)],
            ["lines", arr([int(row.lines[0]), int(row.lines[1])])],
            ["in_degree", int(row.inDegree)],
            ["out_degree", int(row.outDegree)],
          ]),
        ),
      ),
    ],
    [
      "test_gaps",
      arr(
        reader.testGaps([...targets.testGapIds]).map((row) =>
          obj([
            ["id", int(row.id)],
            ["qn", str(row.qn)],
            ["kind", str(row.kind)],
            ["file", str(row.file)],
            ["lines", arr([int(row.lines[0]), int(row.lines[1])])],
            ["tested_depth", opt(row.testedDepth, int)],
          ]),
        ),
      ),
    ],
    [
      "read_node_fts",
      arr(
        reader.readNodeFts(targets.ftsQuery, RANK_LIMIT).map((row) =>
          obj([
            ["id", int(row.id)],
            ["qn", str(row.qn)],
            ["kind", str(row.kind)],
            ["file", str(row.file)],
            ["lines", arr([int(row.lines[0]), int(row.lines[1])])],
            ["score_bits", str(scoreBits(row.score))],
          ]),
        ),
      ),
    ],
  ];
  return values;
}

function skippedText(row: {
  readonly id: number;
  readonly path: string;
  readonly reason: string;
  readonly detail: string | null;
  readonly language: string | null;
}): string {
  return obj([
    ["id", int(row.id)],
    ["path", str(row.path)],
    ["reason", str(row.reason)],
    ["detail", opt(row.detail, str)],
    ["language", opt(row.language, str)],
  ]);
}

function neighbourText(neighbour: {
  readonly edge: {
    readonly id: number;
    readonly srcId: number;
    readonly dstId: number;
    readonly kind: string;
    readonly tier: string;
    readonly fileId: number;
    readonly line: number;
    readonly localName: string;
    readonly evidence: string | null;
  };
  readonly node: NodeRow;
}): string {
  const edge = neighbour.edge;
  return obj([
    [
      "edge",
      obj([
        ["id", int(edge.id)],
        ["src_id", int(edge.srcId)],
        ["dst_id", int(edge.dstId)],
        ["kind", str(edge.kind)],
        ["tier", str(edge.tier)],
        ["file_id", int(edge.fileId)],
        ["line", int(edge.line)],
        ["local_name", str(edge.localName)],
        ["evidence", opt(edge.evidence, str)],
      ]),
    ],
    ["node", nodeText(neighbour.node)],
  ]);
}

function unresolvedText(row: {
  readonly id: number;
  readonly fileId: number;
  readonly path: string;
  readonly line: number;
  readonly rawTarget: string;
  readonly evidenceKind: string | null;
  readonly evidence: string | null;
}): string {
  return obj([
    ["id", int(row.id)],
    ["file_id", int(row.fileId)],
    ["path", str(row.path)],
    ["line", int(row.line)],
    ["raw_target", str(row.rawTarget)],
    ["evidence_kind", opt(row.evidenceKind, str)],
    ["evidence", opt(row.evidence, str)],
  ]);
}

/**
 * The one projection that is not literally a column: the TS row parses `props` and the Rust row
 * keeps its text, so the golden carries the text. `JSON.stringify` of the parsed value must be the
 * stored bytes for that to hold — the indexer writes both, and this proves they still agree before
 * a single row is projected.
 */
function propsRoundTrip(conn: Conn): void {
  const rows = conn.sqlite.query<{ readonly id: number; readonly props: string }, []>(
    "SELECT n.id AS id, n.props AS props FROM node n",
  ).all();
  for (const row of rows) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(row.props);
    } catch (error) {
      throw new Error(`node ${row.id} has props that are not JSON: ${(error as Error).message}`);
    }
    if (JSON.stringify(parsed) !== row.props) {
      throw new Error(
        `node ${row.id} stores props text that does not round-trip (${row.props}); the projection compares the raw column`,
      );
    }
  }
}

// ---------------------------------------------------------------------------------------------
// The index runs.
// ---------------------------------------------------------------------------------------------

function runIndex(
  fixture: FixturePlan,
  repoRoot: string,
  dataDir: string,
  home: string,
  mode: "full" | "fast",
): void {
  const result = spawnSync(
    process.execPath,
    [
      "run",
      CLI,
      "index",
      "--mode",
      mode,
      "--repo-root",
      repoRoot,
      "--data-dir",
      dataDir,
    ],
    {
      // The app root, not the fixture: the CLI resolves its own entry from here.
      cwd: REPO_ROOT,
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      timeout: 240_000,
      env: {
        ...process.env,
        // Hermetic: the registry and settings land in the temp home, never the caller's machine.
        SANGKAR_HOME: home,
        SANGKAR_TELEMETRY: "off",
      },
    },
  );
  if (result.error !== undefined) {
    throw new Error(`${fixture.name}: index could not run: ${result.error.message}`);
  }
  if (result.status !== 0) {
    throw new Error(
      `${fixture.name}: index --mode ${mode} failed (exit ${String(result.status)}):\n${result.stderr}`,
    );
  }
}

/**
 * Indexes one fixture twice — a full publish, then a fast one after the planned deletion — and
 * writes its golden. The second publish is what gives `removed_symbols_for` rows to compare.
 */
async function capture(fixture: FixturePlan): Promise<string> {
  const archive = join(ARCHIVE_ROOT, fixture.name);
  const dataDir = join(DB_ROOT, fixture.name);
  await rm(archive, { recursive: true, force: true });
  await rm(dataDir, { recursive: true, force: true });
  await mkdir(DB_ROOT, { recursive: true });
  await mkdir(ARCHIVE_ROOT, { recursive: true });
  await mkdir(dataDir, { recursive: true });
  await cp(join(FIXTURE_ROOT, fixture.name), archive, { recursive: true });
  const home = await mkdtemp(join(tmpdir(), "sangkar-parity-reader-home-"));

  runIndex(fixture, archive, dataDir, home, "full");
  // The deletion happens in the archive, never in the checked-in fixture.
  await rm(join(archive, fixture.removedFile));
  runIndex(fixture, archive, dataDir, home, "fast");

  const dbPath = join(dataDir, "graph.db");
  const conn = openReader(dbPath);
  let body: string;
  try {
    propsRoundTrip(conn);
    const reader = makeReader(conn);
    const targets = deriveTargets(conn, reader);
    const values = project(reader, targets);
    const names = values.map(([method]) => method);
    for (const method of METHODS) {
      if (!names.includes(method)) {
        throw new Error(`${fixture.name}: the projection has no ${method} entry`);
      }
    }
    if (new Set(names).size !== names.length || names.length !== METHODS.length) {
      throw new Error(
        `${fixture.name}: ${names.length} projections for ${METHODS.length} methods`,
      );
    }
    const entries: Array<[string, string]> = [
      ["fixture", quote(fixture.name)],
      ...renderTargets(targets),
      ["note.removed_file", quote(fixture.removedFile)],
      ...values,
    ];
    body = entries.map(([key, value]) => `${quote(key)}: ${value}`).join(",\n");
  } finally {
    conn.close();
  }

  await mkdir(GOLDEN_DIR, { recursive: true });
  const goldenPath = join(GOLDEN_DIR, `${fixture.name}.json`);
  // One key per line, so a recapture diffs readably and the Rust reader can compare per method
  // without a JSON library: valid JSON, one structural projection per line.
  await writeFile(goldenPath, `{\n${body}\n}\n`);

  const compared = METHODS.length;
  process.stdout.write(`captured ${fixture.name}: ${compared} methods → ${goldenPath}\n`);
  return dbPath;
}

async function main(): Promise<void> {
  await rm(DB_ROOT, { recursive: true, force: true });
  await rm(GOLDEN_DIR, { recursive: true, force: true });

  const databases: Array<{ fixture: FixturePlan; dbPath: string }> = [];
  try {
    for (const fixture of FIXTURES) {
      databases.push({ fixture, dbPath: await capture(fixture) });
    }
  } finally {
    if (process.env.SANGKAR_PARITY_READER_KEEP !== "1") {
      await rm(ARCHIVE_ROOT, { recursive: true, force: true });
    }
  }

  process.stdout.write(
    "\ncompare each database against its golden (one line per fixture):\n" +
      databases
        .map(
          ({ dbPath }) =>
            `SANGKAR_FIXTURE_DB=${dbPath} cargo test --manifest-path rust/Cargo.toml ` +
            "-p sangkar-store --test reader_golden -- --nocapture\n",
        )
        .join(""),
  );
}

await main();
