/**
 * Schema parity and runner contract (`TECH-SPEC.md` §5.4.4, check 2).
 *
 * One empty temp database is migrated by the project runner (`#shared/store/migrate`), and the
 * live SQLite schema is then compared table by table against `getTableConfig()` for every table
 * `src/shared/store/schema.ts` declares: columns in order, column flags, index names and column
 * order, foreign-key targets and actions. Any difference is either a `schema.ts` edit that was
 * never migrated or a migration that says something else — both are defects.
 *
 * The rest of the file pins the runner's own contract on a real database: one ledger, a no-op
 * re-run, the `schema_too_new` refusal, the `rebuild_required` flag, and `node_fts` as FTS5
 * external content over `node_fts_state`.
 */
import { Database } from "bun:sqlite";
import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AppError } from "#shared/errors";
import {
  LEDGER_TABLE,
  listMigrations,
  META_REBUILD_REQUIRED,
  META_SCHEMA_VERSION,
  migrate,
  readSchemaVersion,
  schemaVersion,
} from "#shared/store/migrate";
import * as schema from "#shared/store/schema";

/**
 * `getTableConfig` is Drizzle's own reading of `schema.ts`, so the comparison is against the
 * declared schema instead of a second copy of it. It is loaded through a computed specifier
 * because `tests/boundary.test.ts` (rule 4) forbids a static `drizzle-orm` import outside
 * `src/shared/store/**`; this file is the one sanctioned introspection site (§5.4.4).
 */
const DRIZZLE_SQLITE_CORE = "drizzle-orm/sqlite-core";

interface ColumnConfig {
  readonly name: string;
  getSQLType(): string;
  readonly notNull: boolean;
  readonly primary: boolean;
  readonly hasDefault: boolean;
  readonly default: unknown;
  readonly isUnique: boolean;
  readonly uniqueName: string | undefined;
}

interface IndexConfig {
  readonly config: {
    readonly name: string;
    readonly unique: boolean | undefined;
    readonly columns: readonly { readonly name: string }[];
  };
}

interface ForeignKeyConfig {
  readonly onDelete: string | undefined;
  readonly onUpdate: string | undefined;
  reference(): {
    readonly columns: readonly { readonly name: string }[];
    readonly foreignColumns: readonly { readonly name: string }[];
    readonly foreignTable: never;
  };
}

interface TableConfig {
  readonly name: string;
  readonly columns: readonly ColumnConfig[];
  readonly indexes: readonly IndexConfig[];
  readonly foreignKeys: readonly ForeignKeyConfig[];
}

const { getTableConfig: readTableConfig } = (await import(DRIZZLE_SQLITE_CORE)) as {
  getTableConfig: (table: never) => TableConfig;
};

/** Every table `schema.ts` models, as Drizzle sees it. */
const MODELED_TABLES: readonly TableConfig[] = Object.values(schema).map((table) =>
  readTableConfig(table as never),
);

/**
 * Tables in a migrated database that have no `schema.ts` counterpart, and must not be counted
 * as drift: the runner's ledger, the FTS5 virtual table, and the shadow tables SQLite creates
 * for it. `node_fts_content` is deliberately absent from this list: an external-content table
 * has no content shadow table, so its appearance would mean `node_fts` stopped being one.
 */
const OUTSIDE_SCHEMA_TS: ReadonlySet<string> = new Set([
  LEDGER_TABLE,
  "node_fts",
  "node_fts_config",
  "node_fts_data",
  "node_fts_docsize",
  "node_fts_idx",
]);

interface LiveColumn {
  readonly name: string;
  readonly type: string;
  readonly notnull: number;
  readonly dflt_value: string | null;
  readonly pk: number;
}

interface LiveIndex {
  readonly name: string;
  readonly unique: number;
  readonly origin: string;
}

interface LiveIndexColumn {
  readonly seqno: number;
  readonly cid: number;
  readonly name: string | null;
}

interface LiveForeignKey {
  readonly id: number;
  readonly seq: number;
  readonly table: string;
  readonly from: string;
  readonly to: string | null;
  readonly on_update: string;
  readonly on_delete: string;
}

interface IndexShape {
  readonly unique: boolean;
  readonly columns: readonly string[];
}

const tempDirs: string[] = [];

function openTempDb(): Database {
  const dir = mkdtempSync(join(tmpdir(), "sangkar-schema-"));
  tempDirs.push(dir);
  return new Database(join(dir, "graph.db"));
}

function openMigratedDb(): Database {
  const db = openTempDb();
  migrate(db);
  return db;
}

afterAll(() => {
  for (const dir of tempDirs) {
    rmSync(dir, { recursive: true, force: true });
  }
});

function liveColumns(db: Database, table: string): LiveColumn[] {
  return db.query<LiveColumn, []>(`PRAGMA table_info("${table}")`).all();
}

function liveTableNames(db: Database): string[] {
  const rows = db
    .query<{ name: string }, []>(
      "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .all();
  return rows.map((row) => row.name);
}

/**
 * `DEFAULT ''` comes back as `''` from `PRAGMA table_info` and as `""` from Drizzle, so both
 * sides are reduced to the bare value: a mismatch is still a mismatch, a quoting difference is
 * not.
 */
function normalizeDefault(value: string | null): string | null {
  if (value === null) {
    return null;
  }
  const trimmed = value.trim();
  if (trimmed.length >= 2 && trimmed.startsWith("'") && trimmed.endsWith("'")) {
    return trimmed.slice(1, -1);
  }
  return trimmed;
}

function expectedDefault(column: ColumnConfig): string | null {
  if (!column.hasDefault || column.default === undefined) {
    return null;
  }
  const value: unknown = column.default;
  if (typeof value === "string") {
    return value;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  // A `sql` default cannot be compared against `PRAGMA`; the schema uses none.
  return null;
}

/**
 * Explicitly created indexes plus the unique index a column-level `.unique()` generates.
 * `origin = 'pk'`/`'u'` autoindexes are excluded: they are implied by the columns, which the
 * column comparison already covers.
 */
function expectedIndexes(table: TableConfig): Map<string, IndexShape> {
  const indexes = new Map<string, IndexShape>();
  for (const index of table.indexes) {
    indexes.set(index.config.name, {
      unique: index.config.unique === true,
      columns: index.config.columns.map((column) => column.name),
    });
  }
  for (const column of table.columns) {
    if (column.isUnique && column.uniqueName !== undefined) {
      indexes.set(column.uniqueName, { unique: true, columns: [column.name] });
    }
  }
  return indexes;
}

function liveIndexes(db: Database, table: string): Map<string, IndexShape> {
  const indexes = new Map<string, IndexShape>();
  for (const index of db.query<LiveIndex, []>(`PRAGMA index_list("${table}")`).all()) {
    if (index.origin !== "c") {
      continue;
    }
    const columns = db
      .query<LiveIndexColumn, []>(`PRAGMA index_info("${index.name}")`)
      .all()
      .sort((a, b) => a.seqno - b.seqno)
      .map((column) => column.name ?? "<expression>");
    indexes.set(index.name, { unique: index.unique === 1, columns });
  }
  return indexes;
}

function expectedForeignKeys(table: TableConfig): string[] {
  return table.foreignKeys
    .map((foreignKey) => {
      const reference = foreignKey.reference();
      const from = reference.columns.map((column) => column.name).join(",");
      const target = readTableConfig(reference.foreignTable).name;
      const to = reference.foreignColumns.map((column) => column.name).join(",");
      const onDelete = (foreignKey.onDelete ?? "no action").toLowerCase();
      const onUpdate = (foreignKey.onUpdate ?? "no action").toLowerCase();
      return `${from} -> ${target}(${to}) on delete ${onDelete} on update ${onUpdate}`;
    })
    .sort();
}

function liveForeignKeys(db: Database, table: string): string[] {
  const grouped = new Map<number, LiveForeignKey[]>();
  for (const row of db.query<LiveForeignKey, []>(`PRAGMA foreign_key_list("${table}")`).all()) {
    const group = grouped.get(row.id) ?? [];
    group.push(row);
    grouped.set(row.id, group);
  }
  const foreignKeys: string[] = [];
  for (const group of grouped.values()) {
    const [first] = group.sort((a, b) => a.seq - b.seq);
    if (first === undefined) {
      continue;
    }
    const from = group.map((row) => row.from).join(",");
    const to = group.map((row) => row.to ?? "rowid").join(",");
    foreignKeys.push(
      `${from} -> ${first.table}(${to}) on delete ${first.on_delete.toLowerCase()} on update ${first.on_update.toLowerCase()}`,
    );
  }
  return foreignKeys.sort();
}

describe("runner contract", () => {
  test("applies both streams in order and records one ledger row per file", () => {
    const db = openMigratedDb();
    const names = db
      .query<{ name: string }, []>(`SELECT name FROM ${LEDGER_TABLE} ORDER BY rowid`)
      .all()
      .map((row) => row.name);
    expect(names).toEqual(listMigrations().map((file) => file.name));
    expect(names.length).toBeGreaterThanOrEqual(2);
    expect(names).toContain("0000_init");
    expect(names).toContain("raw/0001_fts");
  });

  test("sets meta.schema_version to the number of committed migration files", () => {
    const db = openTempDb();
    expect(readSchemaVersion(db)).toBe(0);
    const result = migrate(db);
    expect(result.fromVersion).toBe(0);
    expect(result.applied).toEqual(listMigrations().map((file) => file.name));
    expect(result.schemaVersion).toBe(schemaVersion());
    expect(readSchemaVersion(db)).toBe(result.schemaVersion);
    expect(result.rebuildRequired).toBe(false);
  });

  test("a partially applied migration is re-runnable: already-exists errors are swallowed", () => {
    const db = openTempDb();
    // The modeled SQL landed without a ledger row: a hand-applied file, an older tool, or a
    // restored backup. Every statement of `0000_init` must then be skipped, not re-thrown.
    for (const file of listMigrations()) {
      if (file.name === "0000_init") {
        db.exec(readFileSync(file.path, "utf8"));
      }
    }
    expect(readSchemaVersion(db)).toBe(0);

    const result = migrate(db);

    expect(result.applied).toEqual(["0000_init", "raw/0001_fts"]);
    expect(readSchemaVersion(db)).toBe(schemaVersion());
    expect(tableDrift(db, MODELED_TABLES)).toEqual([]);
    expect(columnFlagDrift(db, MODELED_TABLES)).toEqual([]);
  });

  test("a second migrate on the same file applies 0 files and leaves schema_version alone", () => {
    const db = openMigratedDb();
    const before = db
      .query<{ value: string }, [string]>("SELECT value FROM meta WHERE key = ?")
      .get(META_SCHEMA_VERSION);
    const ledgerRows = (): number => {
      const row = db.query<{ n: number }, []>(`SELECT count(*) AS n FROM ${LEDGER_TABLE}`).get();
      return row?.n ?? -1;
    };
    const ledgerBefore = ledgerRows();

    const second = migrate(db);

    expect(second.applied).toEqual([]);
    expect(second.fromVersion).toBe(second.schemaVersion);
    expect(ledgerRows()).toBe(ledgerBefore);
    const after = db
      .query<{ value: string }, [string]>("SELECT value FROM meta WHERE key = ?")
      .get(META_SCHEMA_VERSION);
    expect(after).toEqual(before);
  });

  test("refuses a database whose recorded version is newer, with schema_too_new", () => {
    const db = openMigratedDb();
    const newer = schemaVersion() + 1;
    db.run("UPDATE meta SET value = ? WHERE key = ?", [String(newer), META_SCHEMA_VERSION]);

    let thrown: unknown;
    try {
      migrate(db);
    } catch (e) {
      thrown = e;
    }

    expect(thrown).toBeInstanceOf(AppError);
    expect((thrown as AppError).code).toBe("schema_too_new");
    // Nothing ran: the refusal happens before the first file.
    expect(readSchemaVersion(db)).toBe(newer);
  });

  test("reports meta.rebuild_required so the indexer can force a full index", () => {
    const db = openMigratedDb();
    db.run(
      "INSERT INTO meta (key, value) VALUES (?, '1') ON CONFLICT (key) DO UPDATE SET value = excluded.value",
      [META_REBUILD_REQUIRED],
    );
    const flagged = migrate(db);
    expect(flagged.rebuildRequired).toBe(true);
    expect(flagged.applied).toEqual([]);

    db.run("UPDATE meta SET value = '0' WHERE key = ?", [META_REBUILD_REQUIRED]);
    expect(migrate(db).rebuildRequired).toBe(false);
  });
});

/** Every table `schema.ts` declares is missing from `db`, or has an unexpected table. */
function tableDrift(db: Database, tables: readonly TableConfig[]): string[] {
  const live = new Set(liveTableNames(db));
  const modeled = new Set(tables.map((table) => table.name));
  return [
    ...tables.map((table) => table.name).filter((name) => !live.has(name)),
    ...[...live]
      .filter((name) => !modeled.has(name) && !OUTSIDE_SCHEMA_TS.has(name))
      .map((name) => `${name}: table exists in SQL but not in schema.ts`),
  ];
}

function columnOrderDrift(db: Database, tables: readonly TableConfig[]): string[] {
  const drift: string[] = [];
  for (const table of tables) {
    const live = liveColumns(db, table.name).map((column) => column.name);
    const expected = table.columns.map((column) => column.name);
    if (live.join(",") !== expected.join(",")) {
      drift.push(`${table.name}: [${live.join(",")}] != schema.ts [${expected.join(",")}]`);
    }
  }
  return drift;
}

function columnFlagDrift(db: Database, tables: readonly TableConfig[]): string[] {
  const drift: string[] = [];
  for (const table of tables) {
    const live = new Map(liveColumns(db, table.name).map((column) => [column.name, column]));
    for (const column of table.columns) {
      const actual = live.get(column.name);
      if (actual === undefined) {
        continue; // reported by the name/order check
      }
      const observed = {
        type: actual.type.toLowerCase(),
        notNull: actual.notnull === 1,
        primary: actual.pk > 0,
        defaultValue: normalizeDefault(actual.dflt_value),
      };
      const expected = {
        type: column.getSQLType().toLowerCase(),
        notNull: column.notNull,
        primary: column.primary,
        defaultValue: expectedDefault(column),
      };
      if (JSON.stringify(observed) !== JSON.stringify(expected)) {
        drift.push(
          `${table.name}.${column.name}: ${JSON.stringify(observed)} != ${JSON.stringify(expected)}`,
        );
      }
    }
  }
  return drift;
}

function indexDrift(db: Database, tables: readonly TableConfig[]): string[] {
  const drift: string[] = [];
  for (const table of tables) {
    const expected = expectedIndexes(table);
    const live = liveIndexes(db, table.name);
    for (const [name, shape] of expected) {
      const actual = live.get(name);
      if (actual === undefined) {
        drift.push(`${table.name}: index ${name} missing`);
        continue;
      }
      if (actual.unique !== shape.unique || actual.columns.join(",") !== shape.columns.join(",")) {
        drift.push(
          `${table.name}: index ${name} is ${JSON.stringify(actual)}, schema.ts says ${JSON.stringify(shape)}`,
        );
      }
    }
    for (const name of live.keys()) {
      if (!expected.has(name)) {
        drift.push(`${table.name}: index ${name} exists in SQL but not in schema.ts`);
      }
    }
  }
  return drift;
}

function foreignKeyDrift(db: Database, tables: readonly TableConfig[]): string[] {
  const drift: string[] = [];
  for (const table of tables) {
    const expected = expectedForeignKeys(table);
    const live = liveForeignKeys(db, table.name);
    if (live.join("|") !== expected.join("|")) {
      drift.push(`${table.name}: [${live.join(", ")}] != schema.ts [${expected.join(", ")}]`);
    }
  }
  return drift;
}

describe("schema parity: migrated database vs schema.ts", () => {
  const db = openMigratedDb();

  test("there are at least the eleven modeled tables and each one is present", () => {
    expect(MODELED_TABLES.length).toBeGreaterThanOrEqual(11);
    expect(tableDrift(db, MODELED_TABLES)).toEqual([]);
  });

  test("columns match in name and order", () => {
    expect(columnOrderDrift(db, MODELED_TABLES)).toEqual([]);
  });

  test("column type, NOT NULL, primary key and default match", () => {
    expect(columnFlagDrift(db, MODELED_TABLES)).toEqual([]);
  });

  test("index names, uniqueness and column order match", () => {
    expect(indexDrift(db, MODELED_TABLES)).toEqual([]);
  });

  test("foreign keys match, including their targets and actions", () => {
    expect(foreignKeyDrift(db, MODELED_TABLES)).toEqual([]);
  });

  test("the comparison is not vacuous: a drifted database is reported", () => {
    const drifted = openMigratedDb();
    drifted.run("ALTER TABLE node ADD COLUMN sneaked_in TEXT");
    drifted.run("DROP INDEX idx_node_kind");
    drifted.run("CREATE INDEX idx_node_kind ON node (kind, name)");
    expect(tableDrift(drifted, MODELED_TABLES)).toEqual([]);
    expect(columnOrderDrift(drifted, MODELED_TABLES)).not.toEqual([]);
    expect(indexDrift(drifted, MODELED_TABLES)).not.toEqual([]);
  });
});

describe("node_fts: FTS5 external content over node_fts_state", () => {
  test("a mirrored row becomes searchable through the FTS index", () => {
    const db = openMigratedDb();
    db.run(
      "INSERT INTO node_fts_state (rowid_, name, qn, signature, docstring, props_text) VALUES (?, ?, ?, ?, ?, ?)",
      [7, "resolveBare", "a/b.py::resolveBare", "def resolveBare(x)", "resolve bare calls", "{}"],
    );
    db.run("INSERT INTO node_fts (node_fts) VALUES ('rebuild')");

    const hits = db
      .query<{ rowid: number; name: string; qn: string }, [string]>(
        "SELECT rowid, name, qn FROM node_fts WHERE node_fts MATCH ?",
      )
      .all("resolveBare");
    expect(hits).toEqual([{ rowid: 7, name: "resolveBare", qn: "a/b.py::resolveBare" }]);
  });

  test("content is read back from the mirror, not stored twice", () => {
    const db = openMigratedDb();
    db.run("INSERT INTO node_fts_state (rowid_, name, qn) VALUES (?, ?, ?)", [
      11,
      "onlyInMirror",
      "x.py::onlyInMirror",
    ]);
    // Never indexed: a plain FTS5 table would have nothing to return here.
    expect(
      db.query<{ name: string }, [number]>("SELECT name FROM node_fts WHERE rowid = ?").all(11),
    ).toEqual([{ name: "onlyInMirror" }]);
    expect(db.query<{ n: number }, []>("SELECT count(*) AS n FROM node_fts").get()?.n).toBe(1);
  });

  test("diacritics are folded, so `cafe` finds `café`", () => {
    const db = openMigratedDb();
    db.run("INSERT INTO node_fts_state (rowid_, name, qn, docstring) VALUES (?, ?, ?, ?)", [
      3,
      "Café",
      "x.py::Cafe",
      "naïve",
    ]);
    db.run("INSERT INTO node_fts (node_fts) VALUES ('rebuild')");
    const match = (term: string): number =>
      db
        .query<{ n: number }, [string]>("SELECT count(*) AS n FROM node_fts WHERE node_fts MATCH ?")
        .get(term)?.n ?? -1;
    expect(match("cafe")).toBe(1);
    expect(match("naive")).toBe(1);
    expect(match("absent")).toBe(0);
  });

  test("it is declared unicode61 remove_diacritics 2 over the mirror's five columns", () => {
    const db = openMigratedDb();
    const sql =
      db
        .query<{ sql: string }, [string]>(
          "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .get("node_fts")?.sql ?? "";
    expect(sql).toContain("VIRTUAL TABLE node_fts USING fts5");
    expect(sql).toContain("content = 'node_fts_state'");
    expect(sql).toContain("tokenize = 'unicode61 remove_diacritics 2'");
    expect(liveColumns(db, "node_fts").map((column) => column.name)).toEqual([
      "name",
      "qn",
      "signature",
      "docstring",
      "props_text",
    ]);
  });

  test("the raw stream is the SQL that ships, hand-written and committed", () => {
    const rawDir = join(import.meta.dir, "..", "src", "shared", "store", "migrations", "raw");
    expect(readdirSync(rawDir).filter((file) => file.endsWith(".sql"))).toEqual(["0001_fts.sql"]);
    expect(readFileSync(join(rawDir, "0001_fts.sql"), "utf8")).toContain(
      "CREATE VIRTUAL TABLE node_fts USING fts5",
    );
  });
});
