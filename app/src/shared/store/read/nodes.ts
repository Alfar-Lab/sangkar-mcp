/**
 * Node lookups: the shapes `resolve` and `expand` need (`DATABASE.md` §6), one prepared
 * statement per shape.
 *
 * Raw `sqlite.query` rather than Drizzle builders because `TECH-SPEC.md` §5.4.2 measured a
 * re-built builder at 42.7 µs against 1.7 µs for a prepared statement, and `members` runs once
 * per expanded node; §5.4.3 puts a raw row type at the boundary for exactly this. Every
 * statement comes from `prepared(conn, key, build)`, and both the key and the SQL text depend
 * only on the shape, so a warm call assembles nothing.
 *
 * The row shape stays snake_case and is mapped to {@link NodeRow} before it leaves, so no
 * Drizzle `$inferSelect` type crosses this boundary (`TECH-SPEC.md` §5.4.5).
 */
import { AppError } from "#shared/errors";
import { type Conn, prepared } from "#shared/store/db";

/** One node with the repo-relative path of the file that defines it. */
export interface NodeRow {
  readonly id: number;
  readonly fileId: number;
  /** `file.path`, joined here so no caller pays a second lookup per node. */
  readonly path: string;
  readonly parentId: number | null;
  readonly kind: string;
  readonly subkind: string | null;
  readonly name: string;
  readonly qn: string;
  readonly startLine: number;
  readonly endLine: number;
  readonly signature: string | null;
  readonly docstring: string | null;
  readonly isTest: boolean;
  /** Parsed `node.props`; `{}` when the column still holds its `'{}'` default. */
  readonly props: Readonly<Record<string, unknown>>;
}

/**
 * The `node` columns plus `file.path`, in the order {@link toNodeRow} reads them. Aliased `n`
 * and `f`, and exported with the mapper so `edges.ts` selects a whole node through the same
 * list instead of keeping a second copy that can drift.
 */
export const NODE_SELECT =
  "n.id AS id, n.file_id AS file_id, f.path AS path, n.parent_id AS parent_id, " +
  "n.kind AS kind, n.subkind AS subkind, n.name AS name, n.qn AS qn, " +
  "n.start_line AS start_line, n.end_line AS end_line, n.signature AS signature, " +
  "n.docstring AS docstring, n.is_test AS is_test, n.props AS props";

/** The row {@link NODE_SELECT} produces; declared next to the SQL, mapped to {@link NodeRow}. */
export interface NodeSqlRow {
  readonly id: number;
  readonly file_id: number;
  readonly path: string;
  readonly parent_id: number | null;
  readonly kind: string;
  readonly subkind: string | null;
  readonly name: string;
  readonly qn: string;
  readonly start_line: number;
  readonly end_line: number;
  readonly signature: string | null;
  readonly docstring: string | null;
  readonly is_test: number;
  readonly props: string;
}

const NODE_FROM = " FROM node n JOIN file f ON f.id = n.file_id";

/**
 * `props` is JSON written by the indexer, so malformed text is an indexer bug. It raises
 * instead of degrading to `{}`: an empty object reads as "no decorators, no modifiers", which
 * is a wrong answer where an error is a right one.
 */
function parseProps(nodeId: number, text: string): Readonly<Record<string, unknown>> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    throw new AppError("internal", `node ${nodeId} has props that are not JSON`, {
      hint: "re-index the database; props is written by the indexer",
      cause: e,
    });
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new AppError("internal", `node ${nodeId} has props that are not a JSON object`, {
      hint: "re-index the database; props is written by the indexer",
    });
  }
  return parsed as Record<string, unknown>;
}

/** Maps one SQL row to the domain row; exported because `edges.ts` returns whole nodes too. */
export function toNodeRow(row: NodeSqlRow): NodeRow {
  return {
    id: row.id,
    fileId: row.file_id,
    path: row.path,
    parentId: row.parent_id,
    kind: row.kind,
    subkind: row.subkind,
    name: row.name,
    qn: row.qn,
    startLine: row.start_line,
    endLine: row.end_line,
    signature: row.signature,
    docstring: row.docstring,
    isTest: row.is_test !== 0,
    props: parseProps(row.id, row.props),
  };
}

/** The `findByName` SQL for one kind-filter arity; called only when the shape is a cache miss. */
function byNameSql(kindCount: number): string {
  const kinds = kindCount === 0 ? "" : ` AND n.kind IN (${placeholders(kindCount)})`;
  return `SELECT ${NODE_SELECT}${NODE_FROM} WHERE n.name = ?${kinds} ORDER BY n.qn`;
}

/** `?, ?, ?` for an `IN (...)` list; the arity is part of the cache key, never a per-call loop. */
function placeholders(count: number): string {
  return new Array<string>(count).fill("?").join(", ");
}

/** Exact resolution on the unique `qn`, the first step of every `trace` (§6). */
export function findByQn(conn: Conn, qn: string): NodeRow | null {
  const row = prepared(conn, "nodes.byQn", (sqlite) =>
    sqlite.query<NodeSqlRow, [string]>(`SELECT ${NODE_SELECT}${NODE_FROM} WHERE n.qn = ?`),
  ).get(qn);
  return row === null ? null : toNodeRow(row);
}

/**
 * Bare-name resolution over `idx_node_name_kind` (§6). `kinds` narrows the candidate set and is
 * all-or-nothing: an empty or omitted list means every kind. Results are ordered by `qn`, which
 * is unique, so the order is stable for a caller's own cursor.
 */
export function findByName(conn: Conn, name: string, kinds?: readonly string[]): NodeRow[] {
  const filter = kinds ?? [];
  const statement = prepared(conn, `nodes.byName/${filter.length}`, (sqlite) =>
    sqlite.query<NodeSqlRow, string[]>(byNameSql(filter.length)),
  );
  return statement.all(name, ...filter).map(toNodeRow);
}

/** `expand(what="members")`: the direct children of `parentId`, over `idx_node_parent` (§6). */
export function members(conn: Conn, parentId: number): NodeRow[] {
  const rows = prepared(conn, "nodes.members", (sqlite) =>
    sqlite.query<NodeSqlRow, [number]>(
      `SELECT ${NODE_SELECT}${NODE_FROM} WHERE n.parent_id = ? ORDER BY n.start_line, n.qn`,
    ),
  ).all(parentId);
  return rows.map(toNodeRow);
}

/** Every node a file defines, over `idx_node_file_line` (§6), in source order. */
export function nodesByFile(conn: Conn, fileId: number): NodeRow[] {
  const rows = prepared(conn, "nodes.byFile", (sqlite) =>
    sqlite.query<NodeSqlRow, [number]>(
      `SELECT ${NODE_SELECT}${NODE_FROM} WHERE n.file_id = ? ORDER BY n.start_line, n.qn`,
    ),
  ).all(fileId);
  return rows.map(toNodeRow);
}
