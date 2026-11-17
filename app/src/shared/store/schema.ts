/**
 * The modeled graph schema, transcribed from `DATABASE.md` §3 with the index plan from §4.
 * This file is the source of truth: `migrations/*.sql` is produced by `drizzle-kit generate`
 * and never hand-edited, so a column or index added here is the only way it reaches a database.
 *
 * Not modeled here on purpose: `node_fts` is an FTS5 virtual table and cannot be expressed in
 * Drizzle; it lives in `migrations/raw/` and is owned by the FTS task. The eleven tables below
 * are exactly the ones `DATABASE.md` §3 declares.
 *
 * Integer flags (`is_test`, `worktree_dirty`, `is_entry`, `had_callers`) stay plain INTEGER so
 * the DDL matches §3 literally; the 1/0 view is a read-side concern.
 */
import {
  type AnySQLiteColumn,
  blob,
  index,
  integer,
  sqliteTable,
  text,
  uniqueIndex,
} from "drizzle-orm/sqlite-core";

/** Identity, schema version and runtime state; the only place a schema version lives (§3.1). */
export const meta = sqliteTable("meta", {
  key: text("key").primaryKey(),
  value: text("value").notNull(),
});

/** Publish history; the current generation is `meta.generation` (equivalently `MAX(seq)`). */
export const generation = sqliteTable("generation", {
  id: text("id").primaryKey(),
  seq: integer("seq").notNull().unique(),
  publishedAt: text("published_at").notNull(),
  baseCommit: text("base_commit"),
  worktreeDirty: integer("worktree_dirty").notNull().default(0),
  filesIndexed: integer("files_indexed").notNull(),
  nodes: integer("nodes").notNull(),
  edges: integer("edges").notNull(),
  durationMs: integer("duration_ms").notNull(),
  mode: text("mode").notNull(),
  status: text("status").notNull(),
});

/** Every file the indexer parsed; also the staleness manifest via `sha256`. */
export const file = sqliteTable(
  "file",
  {
    id: integer("id").primaryKey(),
    path: text("path").notNull(),
    language: text("language").notNull(),
    sha256: text("sha256").notNull(),
    size: integer("size").notNull(),
    lineCount: integer("line_count").notNull(),
    parseStatus: text("parse_status").notNull(),
    /** `'12-12,38-38'` when `parse_status` is `partial`. */
    parseErrorRanges: text("parse_error_ranges"),
    indexedGeneration: text("indexed_generation").notNull(),
  },
  (t) => [uniqueIndex("file_path_unique").on(t.path), index("idx_file_language").on(t.language)],
);

/**
 * Every path the indexer looked at and did not index. Without it, "no callers in X" cannot be
 * told apart from "never parsed" (§3.2).
 */
export const notIndexed = sqliteTable(
  "not_indexed",
  {
    id: integer("id").primaryKey(),
    path: text("path").notNull(),
    reason: text("reason").notNull(),
    detail: text("detail"),
    language: text("language"),
  },
  (t) => [
    uniqueIndex("not_indexed_path_reason_unique").on(t.path, t.reason),
    index("idx_not_indexed_path").on(t.path),
  ],
);

/** Call sites whose target did not bind; stored rather than dropped, for absence semantics. */
export const unresolved = sqliteTable(
  "unresolved",
  {
    id: integer("id").primaryKey(),
    fileId: integer("file_id")
      .notNull()
      .references(() => file.id, { onDelete: "cascade" }),
    line: integer("line").notNull(),
    rawTarget: text("raw_target").notNull(),
    evidenceKind: text("evidence_kind"),
    evidence: text("evidence"),
  },
  (t) => [
    uniqueIndex("unresolved_file_id_line_raw_target_unique").on(t.fileId, t.line, t.rawTarget),
    index("idx_unresolved_file").on(t.fileId),
  ],
);

/** Nodes of the symbol graph. `qn` is repo-relative and machine independent. */
export const node = sqliteTable(
  "node",
  {
    id: integer("id").primaryKey(),
    fileId: integer("file_id")
      .notNull()
      .references(() => file.id, { onDelete: "cascade" }),
    parentId: integer("parent_id").references((): AnySQLiteColumn => node.id, {
      onDelete: "cascade",
    }),
    kind: text("kind").notNull(),
    subkind: text("subkind"),
    name: text("name").notNull(),
    qn: text("qn").notNull(),
    startLine: integer("start_line").notNull(),
    endLine: integer("end_line").notNull(),
    signature: text("signature"),
    docstring: text("docstring"),
    isTest: integer("is_test").notNull().default(0),
    props: text("props").notNull().default("{}"),
  },
  (t) => [
    uniqueIndex("node_qn_unique").on(t.qn),
    index("idx_node_name_kind").on(t.name, t.kind),
    index("idx_node_file_line").on(t.fileId, t.startLine),
    index("idx_node_parent").on(t.parentId),
    index("idx_node_kind").on(t.kind),
  ],
);

/**
 * Resolved edges only, id-keyed. `local_name` is part of the dedup key because one
 * `import x, y` statement yields two edges with identical `(src, dst, kind, file, line)`.
 */
export const edge = sqliteTable(
  "edge",
  {
    id: integer("id").primaryKey(),
    srcId: integer("src_id")
      .notNull()
      .references(() => node.id, { onDelete: "cascade" }),
    dstId: integer("dst_id")
      .notNull()
      .references(() => node.id, { onDelete: "cascade" }),
    kind: text("kind").notNull(),
    tier: text("tier").notNull(),
    fileId: integer("file_id")
      .notNull()
      .references(() => file.id, { onDelete: "cascade" }),
    line: integer("line").notNull().default(0),
    localName: text("local_name").notNull().default(""),
    evidence: text("evidence"),
  },
  (t) => [
    uniqueIndex("edge_src_id_dst_id_kind_file_id_line_local_name_unique").on(
      t.srcId,
      t.dstId,
      t.kind,
      t.fileId,
      t.line,
      t.localName,
    ),
    index("idx_edge_dst_kind").on(t.dstId, t.kind, t.tier),
    index("idx_edge_src_kind").on(t.srcId, t.kind, t.tier),
    index("idx_edge_file_line").on(t.fileId, t.line),
    index("idx_edge_kind").on(t.kind),
  ],
);

/** Derived per-node values, computed once at publish so the query path never recomputes them. */
export const nodeMetrics = sqliteTable(
  "node_metrics",
  {
    nodeId: integer("node_id")
      .primaryKey()
      .references(() => node.id, { onDelete: "cascade" }),
    inDegree: integer("in_degree").notNull().default(0),
    outDegree: integer("out_degree").notNull().default(0),
    isEntry: integer("is_entry").notNull().default(0),
    /** Min hops to a test node; NULL means no test is reachable. */
    testedDepth: integer("tested_depth"),
    /** Entry points only: nodes reachable from this one. */
    reachableCount: integer("reachable_count"),
  },
  (t) => [index("idx_metrics_entry").on(t.isEntry), index("idx_metrics_tested").on(t.testedDepth)],
);

/**
 * Removed symbols, so `summary(change)` can report them without re-parsing the base commit.
 * `file_id` carries no foreign key on purpose: a tombstone must outlive the file row.
 */
export const tombstone = sqliteTable("tombstone", {
  qn: text("qn").primaryKey(),
  kind: text("kind").notNull(),
  fileId: integer("file_id"),
  lastSeenGeneration: text("last_seen_generation").notNull(),
  removedInGeneration: text("removed_in_generation").notNull(),
  hadCallers: integer("had_callers").notNull().default(0),
});

/**
 * Mirror for the external-content FTS5 table (`node_fts`, raw SQL): external content cannot
 * delete a row without the indexed values, so they are kept here under the node id.
 */
export const nodeFtsState = sqliteTable("node_fts_state", {
  rowid: integer("rowid_").primaryKey(),
  name: text("name").notNull(),
  qn: text("qn").notNull(),
  signature: text("signature"),
  docstring: text("docstring"),
  propsText: text("props_text"),
});

/** Optional embeddings. Created but empty in the MVP: `meta.embed_dim` NULL means "no vectors". */
export const nodeVector = sqliteTable("node_vector", {
  nodeId: integer("node_id")
    .primaryKey()
    .references(() => node.id, { onDelete: "cascade" }),
  dim: integer("dim").notNull(),
  /** float32 or int8; the format is recorded in `meta.embed_dim`. */
  vec: blob("vec", { mode: "buffer" }).notNull(),
});
