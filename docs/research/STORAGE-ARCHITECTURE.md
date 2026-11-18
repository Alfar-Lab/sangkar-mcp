# Storage architecture: `codebase-memory-mcp` vs `code-review-graph`

> **Source:** [tirth8205/code-review-graph](https://github.com/tirth8205/code-review-graph)
> — CRG 2.4.0, observed at commit `bbdda4b9e4b4` (branch `staging`), Python source.
> **Source:** [DeusData/codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp)
> — CBM `dev`, C/Go source (164 grammars).

Every claim below is stated directly about the analysed software — the Python source of CRG 2.4.0
and the C/Go source of CBM `dev` — with no reference outside this document.

---

## 1. `code-review-graph` — one SQLite file per repository

**Product:** `code-review-graph` (CRG) — version 2.4.0, Python source, one SQLite file per repository.

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

- Data-dir resolution: registry `data_dir` → `CRG_DATA_DIR` → `<repo>/.code-review-graph/`.
- Sidecars are removed on `mode=ro&immutable=1` reads; **no** `-journal` appears, that would mean rollback-journal mode.
- The registry is JSON, not SQLite.

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

| Fact | How it is enforced |
|---|---|
| Edges are keyed by **qualified-name text**, not node ids → renames are reconciled in Python, no FK cascade | edge identity is the `source_qualified` / `target_qualified` text pair; rename reconciliation runs in Python at store time |
| `foreign_keys` is **never enabled** — the FKs declared by v6 were decorative, and v15 dropped those tables | no connection ever turns foreign keys on, so nothing was enforced; the v6 tables are absent from the v15 schema |
| `nodes_fts` is **external-content** FTS5 (`content='nodes'`), `porter unicode61`; column order is BM25 weight order | created with an explicit `content='nodes'` option; the column order is chosen to be the BM25 weight order |
| **No triggers, no views.** FTS sync is application-driven: full `rebuild` or a rowid delta replayed via the mirror table | the schema declares neither a trigger nor a view; every FTS write is issued by the application itself |
| Delta is abandoned for a `rebuild` at `max(500, 0.35 × nodes)` | the delta path compares the pending row count with that threshold and switches to a full rebuild above it |
| Schema version lives in **`metadata`**, not `PRAGMA user_version` | the version is written to and read from the `metadata` table; `PRAGMA user_version` is never stored |
| PRAGMAs: WAL (writers only), `busy_timeout=5000`, `cache_size=-2000` (≈2 MB), `temp_store=FILE`. `synchronous` left at SQLite default **FULL** | applied to the write connection when the database is opened |
| Vector search is a **linear scan** over `embeddings` per provider — no ANN index | one scan over every `embeddings` row per provider; no index structure is ever built |
| Migrations are **forward-only**; v15 is a *shrink* migration (drops 3 tables + 4 indexes) and needs a rebuild to roll back | each version's migration commits in its own transaction and is never reversed; undoing v15 means rebuilding the DB |

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

- `isolation_level=None` → **every statement outside an explicit transaction commits itself** (per-row WAL fsync). Batch transactions exist only in the test harness.
- Incremental = union of three signals: VCS diff, sha256 content mismatch, and pending-re-parse markers. **mtime is never used**.
- Dependents of a changed file are re-written with `store_file_edges` (edges only, node ids kept).
- `build_state` tri-state (`in-progress` / `postprocess-pending` / `complete`) tells you whether a killed build needs a full rebuild or just `postprocess`.

### 1.4 Footprint

| Observation | Value |
|---|---|
| Dominant tables | `edges` (one row per call site) + `idx_edges_composite` (5 columns), `nodes_fts` + full mirror `nodes_fts_state` |
| Measured DB sizes | kubernetes/kubernetes **5.36 GB**, cli/cli **325.9 MB** |
| Size cap / auto-prune / VACUUM | **none** — v15 leaves pages on the freelist by design |
| Only subtractive op | `forget` — deletes rows, re-parses all referrers, re-runs the whole post-process pipeline |

---

## 2. `codebase-memory-mcp` — one SQLite DB per project, two writers, atomic publish

**Product:** `codebase-memory-mcp` (CBM) — version `dev`, C/Go source with 164 grammars.

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

- One DB per **project name**, not per worktree; the project registry IS the `projects` table inside the DB.
- Cache root resolved from `CBM_CACHE_DIR`, defaulting to `~/.cache/codebase-memory-mcp` through per-platform home resolution.
- The team artifact is `VACUUM INTO` → drop indexes → zstd `-3`/`-9` → `.zst` + `artifact.json`; import decompresses, reopens (recreating indexes) and integrity-checks the result. Compression roughly **8–13:1**; a full Linux index is ~14 GB.
- **Three version numbers, deliberately independent**: artifact schema `2`, index format `5`, SQLite `schema_format 4`.

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

| Fact | How it is enforced |
|---|---|
| Edges are **integer-id FK** (`source_id`/`target_id` → `nodes.id`, `ON DELETE CASCADE`) — the opposite choice from CRG | both columns are real foreign keys onto `nodes.id` and the declared cascade is executed by SQLite |
| `foreign_keys = ON` (real enforcement, unlike CRG) | turned on for every connection, so the declared foreign keys are actually enforced by SQLite |
| Dedup key is a **composite with a generated column**: `UNIQUE(source_id, target_id, type, local_name_gen)` — because one IMPORTS statement can carry several local names | a `UNIQUE` constraint over the generated column, so several local names can hang off one IMPORTS row |
| Runtime traces are keyed by **qualified name, not node id**: node ids are reassigned on every reindex, so id-keyed observation would rot | keyed on the qualified-name columns; ids are rewritten on every reindex and cannot serve as the key |
| `nodes_fts` is FTS5 **contentless** (5 stored columns), `unicode61 remove_diacritics 2`; legacy DBs may still have the 4-column shape, so the code probes | created contentless; a legacy 4-column shape is detected at open and reused as-is |
| Metadata tables are deliberately **outside** the graph: `lsp_surface`, `index_coverage`, `index_coverage_meta`, `index_checkout` are "input/metadata about the graph, not part of it" | kept in separate tables that graph traversal never joins |
| Vectors: **int8 × 768**, written by an 11-signal algorithmic encoder, **no model**; a 4-bit RaBitQ-style codec exists too | the encoder quantises to int8 × 768 signals with no model; the 4-bit codec is a separate optional path |
| Vector search = ordered scan + top-K with 5× prefilter — **no ANN index** | an ordered scan with a top-K prefilter; no ANN index structure exists |
| `node_vectors` / `token_vectors` / `store_meta` are created **only by the raw page writer**, not by `init_schema`; on a SQL-path DB vector search degrades to "no vectors" | the page writer emits that DDL itself; `init_schema` does not create these tables |
| `CBM_PAGE_SIZE = 65536` for the raw writer; DBs via `sqlite3_backup` inherit the source page size | the raw writer allocates 65536-byte pages, while an older comment in that same writer still says 4096 — the constant is what runs; a copied DB keeps the page size it was copied from |
| PRAGMAs: `journal_mode=WAL`, `synchronous=NORMAL` (**OFF** during bulk, **FULL** at seal), `cache_size=-65536` (64 MiB), `mmap_size=64 MB`, `busy_timeout=10000`, `temp_store=MEMORY`, `journal_size_limit=256 MiB`, authorizer **denies ATTACH/DETACH** | applied when the connection is opened, and again when the stage is sealed |

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

- Writes are **never in place**: build to a stage file, seal it, `rename()` over the live DB. Readers keep serving the old file; a stale serving DB is preserved when the build fails.
- Incremental: per-file purge, FTS delta append, and `SIMILAR_TO` / `SEMANTICALLY_RELATED` rebuilt **wholesale**.
- Freshness: the watcher polls git HEAD + dirty tree, interval `5 s + 1 s per 500 files`, capped at 60 s.
- Concurrency: single writer via `BEGIN IMMEDIATE` + 10 s `busy_timeout`; readers open `SQLITE_OPEN_READONLY`, skip write PRAGMAs, fall back to `immutable=1` on read-only filesystems; a process-wide mutex guards WAL first-access/recovery.
- Query layer: the Cypher subset is **parsed then executed through direct C store calls** (`cbm_store_find_nodes*`, `cbm_store_find_edges_by_*`) — it is *not* translated into one big SQL join. Ceilings: **100 000 rows** and **30 s**. Large walks switch to an in-RAM CSR adjacency layer.
- Resource guards fail closed and never publish a partial graph: `index_max_files`, `index_max_source_mb`, `index_max_rss_mb`, `index_max_duration_seconds`, single source file ≤ 512 MiB, artifact decompressed ceiling 64 GiB.

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
