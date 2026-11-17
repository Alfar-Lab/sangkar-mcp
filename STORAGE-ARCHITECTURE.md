# Storage architecture: `codebase-memory-mcp` vs `code-review-graph`

Every claim below is traced to source. Verified directly:
`code-review-graph/code_review_graph/graph.py`, `migrations.py`, `search.py`, `embeddings.py`, `locking.py`;
`codebase-memory-mcp/src/store/store.c`, `internal/cbm/sqlite_writer.c`, `src/graph_buffer/`, `src/pipeline/`.

---

## 1. `code-review-graph` — one SQLite file per repository

### 1.1 Physical layout

```mermaid
flowchart TB
    subgraph REPO["Repo working tree (per project)"]
        SRC["source files<br/>35+ languages via Tree-sitter"]
        subgraph DD[".code-review-graph/  — git-ignored"]
            GDB[("graph.db<br/>SQLite · WAL")]
            SIDE["graph.db-wal<br/>graph.db-shm"]
            LOCKF["write.lock<br/>flock LOCK_EX + holder PID"]
            CH["context-history.jsonl<br/>rolls at 1 MiB, keeps 2"]
            EXP["wiki/ · memory/ · graph.json<br/>graph.graphml · graph.svg · obsidian/"]
            IM["install-manifest.json"]
            TOMLc["languages.toml<br/>user config inside an ignored dir"]
        end
    end

    subgraph HOME["~/.code-review-graph  — per user (CRG_HOME)"]
        REG["registry.json<br/>repos: path · alias · data_dir"]
        DS["daemon.pid · daemon.pid.lock<br/>daemon-state.json · watch.toml"]
        LOGS["logs/daemon.log<br/>logs/alias.log"]
    end

    SRC -->|"hash + VCS diff decides re-parse"| GDB
    GDB --- SIDE
    GDB --- LOCKF
    GDB -.- CH
    GDB -.- EXP
    REG -.->|"data_dir override"| DD
    DS --> LOGS
```

- Data-dir resolution: registry `data_dir` → `CRG_DATA_DIR` → `<repo>/.code-review-graph/` (`incremental.py:675-762`).
- Sidecars removed on `mode=ro&immutable=1` reads (`graph.py:1146-1156`); **no** `-journal`, that would mean rollback-journal mode.
- The registry is JSON, not SQLite (`registry.py:20-63`).

### 1.2 ER model of `graph.db` (schema v15)

```mermaid
erDiagram
    nodes {
        int id PK
        text kind "File Class Function Test Type Endpoint YamlPath DocSection ..."
        text name
        text qualified_name UK "path or path::symbol"
        text file_path
        int line_start
        int line_end
        text language
        text parent_name
        text params
        text return_type
        text modifiers
        int is_test
        text file_hash "sha256 of file bytes"
        text extra "JSON: variable metadata"
        text symbol "v10 text after first ::"
        text docstring "v13"
        text name_tokens "v13 camelCase splits"
        int community_id
        real updated_at
    }

    edges {
        int id PK
        text kind "CALLS IMPORTS_FROM TESTED_BY INHERITS ... 19 kinds"
        text source_qualified "TEXT key, not FK"
        text target_qualified "TEXT key, not FK"
        text file_path
        int line
        text extra "JSON: import_scope, source_offset ..."
        real confidence "1.0 / 0.8 / 0.4 / 0.2"
        text confidence_tier "EXTRACTED INFERRED AMBIGUOUS UNRESOLVED"
        text target_resolution "v11 direct | unresolved"
        real updated_at
    }

    metadata {
        text key PK
        text value "schema_version build_state fts_state_synced ..."
    }

    flows {
        int id PK
        text name
        int entry_point_id
        int depth
        int node_count
        int file_count
        real criticality
        text path_json
    }

    flow_memberships {
        int flow_id PK
        int node_id PK
        int position
    }

    communities {
        int id PK
        text name
        int level
        int parent_id
        real cohesion
        int size
        text dominant_language
    }

    nodes_fts {
        text name
        text qualified_name
        text file_path
        text signature
        text docstring
        text name_tokens
    }

    nodes_fts_state {
        int node_id PK
        text file_path
        text name
        text qualified_name
        text docstring
        text name_tokens
    }

    embeddings {
        text qualified_name PK
        blob vector "float32 raw, 384/768/1536"
        text text_hash "skip re-embed if unchanged"
        text provider
    }

    nodes ||--o{ edges : "source_qualified (logical)"
    nodes ||--o{ edges : "target_qualified (logical)"
    flows ||--o{ flow_memberships : "flow_id"
    nodes ||--o{ flow_memberships : "node_id (denormalised, no FK enforced)"
    communities ||--o{ communities : "parent_id"
    communities ||--o{ nodes : "community_id"
    nodes ||--|| nodes_fts : "external content, rowid"
    nodes ||--|| nodes_fts_state : "mirror for delta delete"
    nodes ||--o| embeddings : "qualified_name"
```

**Load-bearing details**

| Fact | Evidence |
|---|---|
| Edges are keyed by **qualified-name text**, not node ids → renames are reconciled in Python, no FK cascade | `graph.py:386-404` |
| `foreign_keys` is **never enabled** — the FKs declared by v6 were decorative, and v15 dropped those tables | `graph.py:1042-1048`, `migrations.py:668-698` |
| `nodes_fts` is **external-content** FTS5 (`content='nodes'`), `porter unicode61`; column order is BM25 weight order | `migrations.py:171-196` |
| **No triggers, no views.** FTS sync is application-driven: full `rebuild` or a rowid delta replayed via the mirror table | `search.py:154, 199-290`; grep for `CREATE TRIGGER`/`CREATE VIEW` = 0 hits |
| Delta is abandoned for a `rebuild` at `max(500, 0.35 × nodes)` | `search.py:106-107` |
| Schema version lives in **`metadata`**, not `PRAGMA user_version` (repo-wide grep: 0 hits) | `migrations.py:130-155` |
| PRAGMAs: WAL (writers only), `busy_timeout=5000`, `cache_size=-2000` (≈2 MB), `temp_store=FILE`. `synchronous` left at SQLite default **FULL** | `graph.py:1042-1048` |
| Vector search is a **linear scan** over `embeddings` per provider — no ANN index | `embeddings.py:1084-1100` |
| Migrations are **forward-only**; v15 is a *shrink* migration (drops 3 tables + 4 indexes) and needs a rebuild to roll back | `migrations.py:701-718`, `CHANGELOG.md:1469-1473` |

### 1.3 Write path

```mermaid
sequenceDiagram
    autonumber
    participant CLI as build / update / watch
    participant LK as write.lock (flock)
    participant VCS as git diff
    participant HASH as sha256 per file
    participant P as Tree-sitter parse pool
    participant DB as graph.db (WAL)

    CLI->>LK: LOCK_EX, write PID (exit 75 if taken)
    CLI->>VCS: git diff --name-status + untracked
    CLI->>HASH: hash every indexed file, compare nodes.file_hash
    VCS-->>CLI: changed ∪ content-mismatch ∪ pending-markers
    CLI->>DB: metadata build_state = in-progress
    loop per file, in parallel parse / serial store
        CLI->>P: parse (process or thread pool)
        P-->>CLI: nodes + edges
        CLI->>DB: BEGIN IMMEDIATE
        CLI->>DB: plan identities, DELETE nodes/edges WHERE file_path
        CLI->>DB: INSERT ... ON CONFLICT(qualified_name) DO UPDATE
        CLI->>DB: INSERT edges (executemany, dedup in Python)
        CLI->>DB: COMMIT  (one transaction per file)
    end
    CLI->>DB: resolver passes: collision endpoints, inheritance, target_resolution (batches of 50000)
    CLI->>DB: FTS delta or rebuild
    CLI->>DB: build_state = complete
    CLI->>LK: release, truncate PID
```

- `isolation_level=None` → **every statement outside an explicit transaction commits itself** (per-row WAL fsync). Batch transactions exist only for tests (`graph.py:1030, 1807-1832`).
- Incremental = union of three signals: VCS diff, sha256 content mismatch, and pending-re-parse markers. **mtime is never used** (`incremental.py:1377-1475, 3173-3420`).
- Dependents of a changed file are re-written with `store_file_edges` (edges only, node ids kept) — `incremental.py:3499-3512`.
- `build_state` tri-state (`in-progress` / `postprocess-pending` / `complete`) tells you whether a killed build needs a full rebuild or just `postprocess` (`build_state.py:41-82`).

### 1.4 Footprint

| Observation | Value |
|---|---|
| Dominant tables | `edges` (one row per call site) + `idx_edges_composite` (5 columns), `nodes_fts` + full mirror `nodes_fts_state` |
| Measured DB sizes | kubernetes/kubernetes **5.36 GB**, cli/cli **325.9 MB** (`CHANGELOG.md:1327-1339`) |
| Size cap / auto-prune / VACUUM | **none** — v15 leaves pages on the freelist by design (`migrations.py:678-690`) |
| Only subtractive op | `forget` — deletes rows, re-parses all referrers, re-runs the whole post-process pipeline (`forget.py:120-199`) |

---

## 2. `codebase-memory-mcp` — one SQLite DB per project, two writers, atomic publish

### 2.1 Physical layout

```mermaid
flowchart TB
    subgraph CACHE["CBM_CACHE_DIR (default ~/.cache/codebase-memory-mcp)"]
        direction TB
        PDB[("PROJECT.db<br/>the graph · WAL")]
        ADR[("PROJECT.db.adr.db<br/>ADR sidecar, survives reindex")]
        STAGE[("PROJECT.db.stage.XXXXXX<br/>+ .stage.lock flock")]
        TMP["PROJECT.db.tmp<br/>dump / VACUUM INTO scratch"]
        CFGDB[("_config.db<br/>runtime_settings + 6 triggers")]
        XREPO[("_cross_repo.db<br/>cross-repo edges")]
        GRANTS["config.json · allowed_roots<br/>approved_manifests"]
        LOGS["logs/cbm-daemon.log<br/>daemon-conflicts.ndjson<br/>activation-events.ndjson"]
        TI["test-impact/base.db<br/>candidate.db · check.db"]
    end

    subgraph REPO["Repo working tree (opt-in)"]
        ART["REPO/.codebase-memory/<br/>graph.db.zst + artifact.json<br/>+ .gitattributes"]
        ENV["REPO/.codebase-memory.json<br/>extension map"]
    end

    PDB -.->|"VACUUM INTO, DROP INDEX, zstd"| ART
    PDB --- ADR
    PDB -.->|"stage → seal → rename"| STAGE
    STAGE -.->|"atomic rename"| PDB
    PDB -.-> XREPO
```

- One DB per **project name**, not per worktree; the project registry IS the `projects` table inside the DB (`store.c:365-369`).
- Cache root resolved in `platform.c:707-733`.
- The team artifact is `VACUUM INTO` → drop indexes → zstd `-3`/`-9` → `.zst` + `artifact.json`; import decompresses, reopens (recreating indexes), integrity-checks (`artifact.c:821-990`, `artifact.h:18`). Compression roughly **8–13:1**; a full Linux index is ~14 GB (README:429).
- **Three version numbers, deliberately independent**: artifact schema `2` (`artifact.h:18`), index format `5` (`store.h:39`), SQLite `schema_format 4` (`sqlite_writer.c:74-76`).

### 2.2 ER model

```mermaid
erDiagram
    projects {
        text name PK
        text indexed_at
        text root_path
    }

    file_hashes {
        text project PK
        text rel_path PK
        text sha256
        int mtime_ns
        int size
    }

    nodes {
        int id PK
        text project FK
        text label "27 labels"
        text name
        text qualified_name UK
        text file_path
        int start_line
        int end_line
        text properties "JSON"
    }

    edges {
        int id PK
        text project FK
        int source_id FK
        int target_id FK
        text type "1 of ~19 types"
        text properties "JSON"
        text url_path_gen "GENERATED json_extract"
        text local_name_gen "GENERATED, IMPORTS only"
    }

    lsp_surface {
        text project PK
        text rel_path PK
        text surface_sha
        text defs_json
        blob ref_bloom
        text config_ctx
    }

    index_coverage {
        text project PK
        text rel_path PK
        text kind PK "parse_partial | parse_unusable | read | extract | oversized"
        text detail
    }

    index_coverage_meta {
        text project PK
        text generation
        text index_mode
        text recorded_at
        text recording_status
        int ignored_files_stored
        int coverage_version
        int hash_records_complete
        int unresolved_calls_complete
    }

    index_checkout {
        text project PK
        text indexed_checkout_sha
    }

    observed_calls {
        text project PK
        text caller_qn PK
        text callee_qn PK
        text label PK
        int count
        text first_seen
        text last_seen
    }

    observed_paths {
        text project PK
        text label PK
        text path_json PK
        int count
    }

    nodes_fts {
        text name
        text qualified_name
        text label
        text file_path
        text body
    }

    node_vectors {
        int node_id PK
        text project
        blob vector "int8 × 768"
    }

    token_vectors {
        int id PK
        text project
        text token
        blob vector
        int idf "×1000"
    }

    store_meta {
        text k PK
        text v "db_uid · mutation_gen"
    }

    projects ||--o{ file_hashes : "CASCADE"
    projects ||--o{ nodes : "CASCADE"
    projects ||--o{ edges : "CASCADE"
    nodes ||--o{ edges : "source_id CASCADE"
    nodes ||--o{ edges : "target_id CASCADE"
    projects ||--o| index_coverage_meta : ""
    projects ||--o{ index_coverage : ""
    index_coverage ||--o{ index_unresolved_candidates : "CASCADE"
    projects ||--o| index_checkout : ""
    projects ||--o{ observed_calls : "by qualified_name"
    projects ||--o{ observed_paths : ""
    projects ||--o{ lsp_surface : "CASCADE"
    nodes ||--o| node_vectors : "node_id"
    nodes ||--o| nodes_fts : "contentless FTS5"
    nodes ||--o| store_meta : ""
```

**Load-bearing details**

| Fact | Evidence |
|---|---|
| Edges are **integer-id FK** (`source_id`/`target_id` → `nodes.id`, `ON DELETE CASCADE`) — the opposite choice from CRG | `store.c:398-409` |
| `foreign_keys = ON` (real enforcement, unlike CRG) | `store.c:770` |
| Dedup key is a **composite with a generated column**: `UNIQUE(source_id, target_id, type, local_name_gen)` — because one IMPORTS statement can carry several local names | `store.c:378-409` + comment |
| Runtime traces are keyed by **qualified name, not node id**: node ids are reassigned on every reindex, so id-keyed observation would rot | `store.c:482-505` + comment |
| `nodes_fts` is FTS5 **contentless** (5 stored columns), `unicode61 remove_diacritics 2`; legacy DBs may still have the 4-column shape, so the code probes | `store.c:591-645` |
| Metadata tables are deliberately **outside** the graph: `lsp_surface`, `index_coverage`, `index_coverage_meta`, `index_checkout` are "input/metadata about the graph, not part of it" | `store.c:424-527` + comments |
| Vectors: **int8 × 768**, written by an 11-signal algorithmic encoder, **no model**; a 4-bit RaBitQ-style codec exists too | `semantic.h:39-40`, `rotsq.h:49-55`, `pass_semantic_edges.c:1411-1414` |
| Vector search = ordered scan + top-K with 5× prefilter — **no ANN index** | `store.c:908-940, 12912-12975` |
| `node_vectors` / `token_vectors` / `store_meta` are created **only by the raw page writer**, not by `init_schema`; on a SQL-path DB vector search degrades to "no vectors" | `sqlite_writer.c:2315-2322`, `store.c:12880-12890` |
| `CBM_PAGE_SIZE = 65536` for the raw writer (its own header comment still says 4096 — code wins); DBs via `sqlite3_backup` inherit the source page size | `sqlite_writer.c:7 vs :48`, `store.c:2628-2700` |
| PRAGMAs: `journal_mode=WAL`, `synchronous=NORMAL` (**OFF** during bulk, **FULL** at seal), `cache_size=-65536` (64 MiB), `mmap_size=64 MB`, `busy_timeout=10000`, `temp_store=MEMORY`, `journal_size_limit=256 MiB`, authorizer **denies ATTACH/DETACH** | `store.c:757-836, 1026-1036` |

### 2.3 Write path — in-memory buffer, raw page writer, atomic publish

```mermaid
sequenceDiagram
    autonumber
    participant PIPE as pipeline
    participant GBUF as graph_buffer (RAM, temp ids)
    participant W as sqlite_writer raw page writer
    participant ST as PROJECT.db.stage
    participant ADM as daemon admission
    participant FIN as final PROJECT.db

    PIPE->>ADM: claim build (conflicting build rejected)
    PIPE->>GBUF: parse + populate nodes/edges (temp ids), 65536-node partitions
    alt primary publish path
        GBUF->>W: dump partitions to pages
        W->>ST: sqlite_master + tables + indexes + fsync
    else SQL path small or incremental
        GBUF->>ST: upsert_project, begin_bulk (sync=OFF, 64 MiB cache)
        GBUF->>ST: drop_indexes
        GBUF->>ST: BEGIN IMMEDIATE, delete project rows, INSERT ON CONFLICT ... RETURNING id
        GBUF->>ST: edges json_patch upsert, COMMIT, create_indexes, end_bulk
    end
    PIPE->>ST: publish_staged (mutate stage via SQL)
    PIPE->>ST: finalize: WAL checkpoint TRUNCATE, journal_mode=DELETE, synchronous=FULL
    PIPE->>FIN: atomic rename over the live file, delete stale -wal/-shm/-journal
```

- Writes are **never in place**: build to a stage file, seal it, `rename()` over the live DB. Readers keep serving the old file (a stale serving DB is preserved on failure — `INDEX_RESOURCE_LIMITS.md:97-112`).
- Incremental: per-file purge (`pipeline_incremental.c:1148`), FTS delta append (`pipeline_delta.c:1051`), and `SIMILAR_TO` / `SEMANTICALLY_RELATED` rebuilt **wholesale** (`pipeline_delta.c:1190-1203`).
- Freshness: the watcher polls git HEAD + dirty tree, interval `5 s + 1 s per 500 files`, capped at 60 s (`watcher.h:1-10`).
- Concurrency: single writer via `BEGIN IMMEDIATE` + 10 s `busy_timeout`; readers open `SQLITE_OPEN_READONLY`, skip write PRAGMAs, fall back to `immutable=1` on read-only filesystems; a process-wide mutex guards WAL first-access/recovery (`store.c:1208-1288`).
- Query layer: the Cypher subset is **parsed then executed through direct C store calls** (`cbm_store_find_nodes*`, `cbm_store_find_edges_by_*`) — it is *not* translated into one big SQL join. Ceilings: **100 000 rows** and **30 s** (`cypher.c:3249-3261`). Large walks switch to an in-RAM CSR adjacency layer (`graph_mem.h:1-30`).
- Resource guards fail closed and never publish a partial graph: `index_max_files`, `index_max_source_mb`, `index_max_rss_mb`, `index_max_duration_seconds`, single source file ≤ 512 MiB, artifact decompressed ceiling 64 GiB (`docs/INDEX_RESOURCE_LIMITS.md`, `artifact.c:20-23`).

---

## 3. Head-to-head

```mermaid
flowchart LR
    subgraph CRG["code-review-graph"]
        C1["1 SQLite file per repo<br/>graph.db"]
        C2["text-keyed edges<br/>qualified_name"]
        C3["in-place writes<br/>BEGIN IMMEDIATE per file"]
        C4["app-driven FTS + mirror table"]
        C5["schema_version row in metadata"]
        C6["linear vector scan in-DB"]
        C7["no size cap, no VACUUM"]
    end
    subgraph CBM["codebase-memory-mcp"]
        B1["1 DB per project + sidecars<br/>+ opt-in .zst artifact"]
        B2["id-keyed edges<br/>FK CASCADE, enforced"]
        B3["RAM buffer → stage → seal<br/>→ atomic rename"]
        B4["contentless FTS5<br/>+ delta append"]
        B5["user_version / schema_format<br/>+ artifact schema, independent"]
        B6["int8 vectors from an encoder<br/>linear scan + RaBitQ codec"]
        B7["documented limits, fail-closed"]
    end
    CRG -->|"smaller blast radius,<br/>simpler recovery"| CBM
    CBM -->|"crash-safe publish,<br/>multi-agent concurrency"| CRG
```

| Dimension | `code-review-graph` | `codebase-memory-mcp` |
|---|---|---|
| Engine | SQLite, WAL, page size default 4096 | SQLite 3.46 vendored; raw writer uses 65536 |
| Unit of storage | 1 DB per repo, `.code-review-graph/graph.db` | 1 DB per project + ADR sidecar + `_config.db` + `_cross_repo.db` |
| Edge identity | qualified-name **text** (no FK) | integer **node ids** with enforced `ON DELETE CASCADE` |
| Schema versioning | `metadata.schema_version` → 15 | SQLite `user_version` / `schema_format 4`, index format 5, artifact schema 2 |
| Migrations | forward-only, per-version transaction, idempotent, v15 shrinks the schema | `init_schema` `CREATE IF NOT EXISTS` + compatibility probes for legacy shapes |
| Write safety | `flock` file lock + `BEGIN IMMEDIATE`; a killed build leaves `build_state` to repair | stage → seal → atomic rename; partial graph never published |
| Bulk write optimisation | drop/rebuild targeted indexes per migration; `_UPDATE_BATCH=50 000` | `begin_bulk`: `sync=OFF`, 64 MiB cache, drop → insert → recreate indexes |
| FTS | external-content + mirror table, app-synced, delta or rebuild at 35 % | contentless FTS5, delta append, shape probe for legacy DBs |
| Vectors | float32 BLOB, provider-dependent dims, `text_hash` reuse | int8 × 768, self-contained encoder, IDF table, optional 4-bit codes |
| Footprint control | none built in; measured up to 5.36 GB | opt-in zstd artifact (8–13:1) + documented hard limits |
| Concurrency model | single process, serial stores, write lock with exit 75 | per-account daemon, admission barrier, single writer per DB, read-only replicas |

**The architectural split in one sentence:** CRG treats the DB as a **derived cache that can always be rebuilt from git** (cheap, text-keyed, in-place, no size discipline); CBM treats it as a **publishable artifact** (id-keyed, staged, sealed, compressible, shareable) — which is why CBM pays for a hand-written SQLite page writer and CRG does not.
