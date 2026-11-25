#!/usr/bin/env bun
/**
 * The census of one `graph.db`: every count a written generation can be held to, plus the *order* of
 * the six row sequences a page is taken from.
 *
 * `rust/tests/golden/census/<fixture>.ts.json` is the TypeScript indexer's census of a fixture, and
 * `cargo test -p sangkar-index --test census` builds the same corpus with the Rust worker, computes
 * the same document from the file that worker wrote, and prints a per-key delta table. Counts alone
 * would pass a writer that inserts the same rows in a different order — a cursor is a position in a
 * generation's stable ordering (`src/shared/cursor.ts`) and rowids are minted by the writer — so the
 * six list-producing queries carry their ids element by element: node ids, edge ids,
 * `node_fts_state` rowids, `entryPoints(5)`, `hotspots(5)` and `findByName` for a duplicated name.
 *
 * Two usages:
 *
 *   bun run scripts/parity/census.ts <graph.db>
 *       one compact JSON line: the census of any published database, whichever implementation wrote
 *       it. `src/tools/status/handler.ts` and the reader are the only SQL it repeats, so a count
 *       that moves here is a count every tool would have served.
 *
 *   bun run scripts/parity/census.ts --capture <polyglot|broken> --rust-sqlite-version <v>
 *       archive the fixture to a temp directory, index it with `bin/sangkar.ts index --mode full
 *       --force`, and write `rust/tests/golden/census/<fixture>.ts.json`. The archive is the same
 *       mechanic `capture-goldens.ts` uses: indexing a checked-in fixture in place would let a run
 *       write inside the corpus, and the two implementations would then be compared over trees with
 *       different histories.
 *
 * What the document must not carry: an absolute path, a timestamp, or a generation id. The id is
 * random per publish (`g-` plus three random bytes), so the facts that survive it are `seq`, `mode`
 * and `status`; two implementations indexing two different temp trees then produce the same bytes.
 *
 * `sqlite_version` is the one key the two sides never share. bun links its own SQLite, the Rust
 * binary links `libsqlite3-sys`'s bundled one, and FTS5's `bm25` scoring and its tie-breaks are
 * version quantities — the `bm25 ASC, in_degree DESC, id ASC` ordering `search` ranks by depends on
 * them. So the census records the version it read instead of hiding it, and the golden records the
 * Rust side's version too (`rust_sqlite_version`), measured by running this script on a database
 * that side built. A dependency bump is then a visible re-pin, not an invisible drift.
 *
 * `allowed_delta` is the one field a capture does not derive. It is a reviewed judgement about a
 * grammar substitution (TASK-038), preserved across recaptures and checked for its exact value:
 * `{language, grammar, reason, deltas: {"<row path>": <int>}}`, where a row path is the census key
 * the delta is allowed on (`per_language.c.nodes`, `nodes`, `nodes_by_kind.function_definition`, …).
 * An ordering difference is never allowed, and neither is a delta this list does not name.
 */
import { spawnSync } from "node:child_process";
import { cp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { type Conn, openReader } from "#shared/store/db";
import { makeReader } from "#shared/store/read/reader";

const REPO_ROOT = resolve(import.meta.dir, "..", "..");
const CLI = join(REPO_ROOT, "bin", "sangkar.ts");
const FIXTURES = join(REPO_ROOT, "rust", "tests", "parity", "fixtures");
const GOLDENS = join(REPO_ROOT, "rust", "tests", "golden", "census");
const WORK_ROOT = join(tmpdir(), "sangkar-parity-census");

/** The key every capture runs with, so a census run is hermetic and mints nothing a real one would. */
const CURSOR_KEY = "sangkar-parity-fixed-cursor-key-0123456789abcdef";

/**
 * The bound `summary(repo)` and `search` rank with. Five is small enough that the top of a fixture's
 * ranking is a strict order and the cut has a tie, which is what makes the sequence worth pinning.
 */
const PAGE = 5;

/** The fixtures a golden is captured for, in report order. */
const FIXTURE_NAMES = ["polyglot", "broken"] as const;
type FixtureName = (typeof FIXTURE_NAMES)[number];

/** One `GROUP BY` result: the key the query grouped on, and the rows that landed in it. */
interface Counter {
  readonly key: string;
  readonly rows: number;
}

/** `{k: v}` with the keys sorted, so two writers emit the same text for the same counts. */
function ordered(entries: Iterable<readonly [string, number]>): Record<string, number> {
  const sorted = [...entries].sort((left, right) =>
    left[0] < right[0] ? -1 : left[0] > right[0] ? 1 : 0,
  );
  const out: Record<string, number> = {};
  for (const [key, value] of sorted) {
    out[key] = value;
  }
  return out;
}

/** Every row of a statement; the connection is the caller's, so the SQL is the only thing here. */
function rows<T extends object>(conn: Conn, sql: string): T[] {
  return conn.sqlite.query<T, []>(sql).all();
}

/** One row of a statement; a statement that returns none is a database that cannot answer. */
function single<T extends object>(conn: Conn, label: string, sql: string): T {
  const row = conn.sqlite.query<T, []>(sql).get();
  if (row === null || row === undefined) {
    throw new Error(`${label}: the statement returned no row`);
  }
  return row;
}

/** One integer answer, which is what every scalar in this document is except the version. */
function count(conn: Conn, label: string, sql: string): number {
  return single<{ value: number }>(conn, label, sql).value;
}

/** The ids one statement returns, in the order the statement asked for. */
function idList(conn: Conn, sql: string): number[] {
  return rows<{ id: number }>(conn, sql).map((row) => row.id);
}

/** One key/value count per language, from the table the count lives in. */
function byLanguage(conn: Conn, sql: string): Map<string, number> {
  const out = new Map<string, number>();
  for (const row of rows<{ language: string; rows: number }>(conn, sql)) {
    out.set(row.language, row.rows);
  }
  return out;
}

/**
 * The name `findByName` is probed with: the lexicographically first name more than one node carries,
 * chosen from the database itself so both sides probe the same symbol and the answer is the
 * reader's ordering rather than a fixture constant. `null` when nothing is duplicated.
 */
function duplicatedName(conn: Conn): string | null {
  const row = conn.sqlite
    .query<{ name: string }, []>(
      "SELECT name AS name FROM node GROUP BY name HAVING count(*) > 1 ORDER BY name LIMIT 1",
    )
    .get();
  return row?.name ?? null;
}

/** The census document, in the fixed key order both implementations emit. */
function census(conn: Conn): Record<string, unknown> {
  const reader = makeReader(conn);
  const files = reader.allFiles();
  const skipped = reader.notIndexedAll();
  const unresolved = reader.unresolvedForFiles(files.map((file) => file.id));
  const generation = reader.latestGeneration();
  if (generation === null) {
    throw new Error("census: the database carries no generation row, so nothing was published");
  }

  // `file.language` is the row's own value, so a node counted by language is counted by the file
  // that defines it. A `not_indexed` path has no `file` row and is reported under its reason only —
  // `status`'s `{language, files, parsed, failed}` census is the one that folds the two together.
  const languageOf = new Map(files.map((file) => [file.id, file.language] as const));
  const filesByLanguage = new Map<string, number>();
  for (const file of files) {
    filesByLanguage.set(file.language, (filesByLanguage.get(file.language) ?? 0) + 1);
  }
  const nodesByLanguage = byLanguage(
    conn,
    "SELECT f.language AS language, count(*) AS rows FROM node n JOIN file f ON f.id = n.file_id" +
      " GROUP BY f.language",
  );
  const edgesByLanguage = byLanguage(
    conn,
    "SELECT f.language AS language, count(*) AS rows FROM edge e JOIN file f ON f.id = e.file_id" +
      " GROUP BY f.language",
  );
  const unresolvedByLanguage = new Map<string, number>();
  const unresolvedByKind = new Map<string, number>();
  for (const row of unresolved) {
    const language = languageOf.get(row.fileId);
    if (language !== undefined) {
      unresolvedByLanguage.set(language, (unresolvedByLanguage.get(language) ?? 0) + 1);
    }
    const kind = row.evidenceKind ?? "none";
    unresolvedByKind.set(kind, (unresolvedByKind.get(kind) ?? 0) + 1);
  }

  const languages = new Set<string>([
    ...filesByLanguage.keys(),
    ...nodesByLanguage.keys(),
    ...edgesByLanguage.keys(),
    ...unresolvedByLanguage.keys(),
  ]);
  const perLanguage: Record<string, Record<string, number>> = {};
  for (const language of [...languages].sort()) {
    perLanguage[language] = {
      files: filesByLanguage.get(language) ?? 0,
      nodes: nodesByLanguage.get(language) ?? 0,
      edges: edgesByLanguage.get(language) ?? 0,
      unresolved: unresolvedByLanguage.get(language) ?? 0,
    };
  }

  const duplicateName = duplicatedName(conn);

  return {
    sqlite_version: single<{ value: string }>(conn, "sqlite_version", "SELECT sqlite_version() AS value")
      .value,
    generation: { seq: generation.seq, mode: generation.mode, status: generation.status },
    files: files.length,
    nodes: count(conn, "count node", "SELECT count(*) AS value FROM node"),
    edges: count(conn, "count edge", "SELECT count(*) AS value FROM edge"),
    unresolved: unresolved.length,
    per_language: perLanguage,
    nodes_by_kind: ordered(
      rows<Counter>(conn, "SELECT kind AS key, count(*) AS rows FROM node GROUP BY kind").map(
        (row) => [row.key, row.rows] as const,
      ),
    ),
    // Derived from `per_language` on purpose: one query, and the flat map cannot drift from the
    // grouped one that the per-language acceptance is measured on.
    nodes_by_language: ordered(
      Object.entries(perLanguage).map(([language, counts]) => [language, counts.nodes] as const),
    ),
    edges_by_kind_tier: ordered(
      rows<Counter>(
        conn,
        "SELECT kind || ':' || tier AS key, count(*) AS rows FROM edge GROUP BY kind, tier",
      ).map((row) => [row.key, row.rows] as const),
    ),
    unresolved_by_kind: ordered(unresolvedByKind),
    not_indexed_by_reason: ordered(
      skipped.reduce((counts, row) => {
        counts.set(row.reason, (counts.get(row.reason) ?? 0) + 1);
        return counts;
      }, new Map<string, number>()),
    ),
    duplicate_name: duplicateName,
    order: {
      node_ids: idList(conn, "SELECT id AS id FROM node ORDER BY id"),
      edge_ids: idList(conn, "SELECT id AS id FROM edge ORDER BY id"),
      node_fts_state_rowids: idList(conn, "SELECT rowid_ AS id FROM node_fts_state ORDER BY rowid_"),
      entry_point_ids: reader.entryPoints(PAGE).map((row) => row.id),
      hotspot_ids: reader.hotspots(PAGE).map((row) => row.id),
      duplicate_name_ids:
        duplicateName === null
          ? []
          : reader.findByName(duplicateName).map((row) => row.id),
    },
  };
}

/** Open the file, read it, close it: a census must not leave a handle on someone else's database. */
function censusOf(path: string): string {
  if (!existsSync(path)) {
    throw new Error(`census: ${path} does not exist`);
  }
  const conn = openReader(path);
  try {
    return JSON.stringify(census(conn));
  } finally {
    conn.close();
  }
}

/** One CLI run, stdout and stderr split: stderr is logging and never part of a census. */
function runCli(
  argv: readonly string[],
  home: string,
  repoRoot: string,
  dataDir: string,
): { stdout: string; stderr: string; code: number } {
  const result = spawnSync(
    process.execPath,
    ["run", CLI, ...argv, "--repo-root", repoRoot, "--data-dir", dataDir],
    {
      // The app root, not the fixture: the CLI resolves its own entry and app paths from here.
      cwd: REPO_ROOT,
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      timeout: 240_000,
      env: {
        ...process.env,
        // Every machine-level write (registry, settings) lands in the temp home, never the caller's.
        SANGKAR_HOME: home,
        SANGKAR_CURSOR_KEY: CURSOR_KEY,
        // Hermetic: a capture must not append to a telemetry file, and no census reads one.
        SANGKAR_TELEMETRY: "off",
      },
    },
  );
  if (result.error !== undefined) {
    throw new Error(`${argv[0]} could not run: ${result.error.message}`);
  }
  return { stdout: result.stdout ?? "", stderr: result.stderr ?? "", code: result.status ?? -1 };
}

/** The reviewed allowances already in a golden, kept across a recapture. */
async function existingAllowances(path: string): Promise<unknown[]> {
  if (!existsSync(path)) {
    return [];
  }
  const parsed: unknown = JSON.parse(await readFile(path, "utf8"));
  if (typeof parsed !== "object" || parsed === null) {
    throw new Error(`${path} is not a census golden`);
  }
  const allowed = (parsed as { allowed_delta?: unknown }).allowed_delta;
  if (allowed === undefined) {
    return [];
  }
  if (!Array.isArray(allowed)) {
    throw new Error(`${path}: allowed_delta is not a list`);
  }
  return allowed;
}

/** Archive, index and write the golden for one fixture. */
async function capture(fixture: FixtureName, rustSqliteVersion: string): Promise<void> {
  const root = join(WORK_ROOT, fixture);
  const repoRoot = join(root, fixture);
  const dataDir = join(root, "data");
  const home = join(root, "home");
  await rm(root, { recursive: true, force: true });
  await cp(join(FIXTURES, fixture), repoRoot, { recursive: true });
  await mkdir(dataDir, { recursive: true });
  await mkdir(home, { recursive: true });

  // One publish, before any read: every count and every id below belongs to a generation the
  // TypeScript indexer wrote, and the mode the Rust side must be built with is the same one.
  const run = runCli(["index", "--mode", "full", "--force"], home, repoRoot, dataDir);
  if (run.code !== 0) {
    throw new Error(`${fixture}: the index run failed (exit ${run.code}):\n${run.stderr}`);
  }
  if (run.stdout !== "") {
    throw new Error(`${fixture}: the index verb wrote to stdout, which it never does`);
  }

  const path = join(GOLDENS, `${fixture}.ts.json`);
  const conn = openReader(join(dataDir, "graph.db"));
  let document: Record<string, unknown>;
  try {
    document = census(conn);
  } finally {
    conn.close();
  }

  const golden = {
    fixture,
    source: "bin/sangkar.ts index --mode full --force",
    rust_sqlite_version: rustSqliteVersion,
    allowed_delta: await existingAllowances(path),
    census: document,
  };
  await mkdir(GOLDENS, { recursive: true });
  await writeFile(path, `${JSON.stringify(golden, null, 2)}\n`);

  const order = document.order as Record<string, number[]>;
  const counts = (key: string): number => document[key] as number;
  process.stdout.write(
    `wrote rust/tests/golden/census/${fixture}.ts.json: ${counts("files")} files, ` +
      `${counts("nodes")} nodes, ${counts("edges")} edges, ` +
      `${order.node_ids.length + order.edge_ids.length} ids ordered, ` +
      `sqlite ${String(document.sqlite_version)}\n`,
  );
}

const USAGE = `usage:
  bun run scripts/parity/census.ts <graph.db>
      the census of one published database, as one compact JSON line
  bun run scripts/parity/census.ts --capture <${FIXTURE_NAMES.join("|")}> --rust-sqlite-version <v>
      recapture rust/tests/golden/census/<fixture>.ts.json; <v> is the Rust side's
      \`select sqlite_version()\`, read by running this script on a Rust-built database`;

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const flags = new Map<string, string>();
  const positional: string[] = [];
  for (let index = 0; index < args.length; index += 1) {
    const arg = args[index] ?? "";
    if (!arg.startsWith("--")) {
      positional.push(arg);
      continue;
    }
    const value = args[index + 1];
    if (value === undefined || value.startsWith("--")) {
      throw new Error(`${arg} needs a value\n${USAGE}`);
    }
    flags.set(arg, value);
    index += 1;
  }

  const fixture = flags.get("--capture");
  if (fixture === undefined) {
    if (positional.length !== 1) {
      throw new Error(USAGE);
    }
    process.stdout.write(`${censusOf(positional[0] ?? "")}\n`);
    return;
  }

  if (!(FIXTURE_NAMES as readonly string[]).includes(fixture)) {
    throw new Error(`--capture: unknown fixture ${fixture}; the fixtures are ${FIXTURE_NAMES.join(", ")}`);
  }
  if (positional.length !== 0) {
    throw new Error(`--capture takes no positional argument\n${USAGE}`);
  }
  const rustSqliteVersion = flags.get("--rust-sqlite-version");
  if (rustSqliteVersion === undefined) {
    // Required rather than optional: a golden without the Rust build's version could not tell a
    // dependency bump from a writer change, and FTS5 ordering is a version quantity.
    throw new Error(`--capture needs --rust-sqlite-version\n${USAGE}`);
  }

  await mkdir(WORK_ROOT, { recursive: true });
  try {
    await capture(fixture as FixtureName, rustSqliteVersion);
  } finally {
    if (process.env.SANGKAR_PARITY_CENSUS_KEEP !== "1") {
      await rm(WORK_ROOT, { recursive: true, force: true });
    }
  }
}

await main();
