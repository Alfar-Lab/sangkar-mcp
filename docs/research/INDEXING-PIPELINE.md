# Indexing pipeline pseudocode: `code-review-graph` vs `codebase-memory-mcp`

> **Source:** [tirth8205/code-review-graph](https://github.com/tirth8205/code-review-graph) — CRG 2.4.0, observed at commit `bbdda4b9e4b4` (branch `staging`), Python.
> [DeusData/codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp) — CBM `dev`, C/Go (164 grammars).

Every statement below was read directly from each project's source. Anchor facts:
`MIN_FILES_FOR_PARALLEL` is 50; CRG parses serially below 8 files; `_MAX_PARSE_WORKERS` is
`CRG_PARSE_WORKERS` or `min(cpu, 8)`; `cbm_pipeline_pass_githistory` has **zero callers**; no `index`
CLI subcommand exists.

---

# 1. `code-review-graph` — pull-based, one file at a time, in place

## 1.1 Entry: `build`

```
build_or_update_graph(full_rebuild=True, postprocess, wait_for_lock)
  root    = resolve_root(repo_root)
  db_path = get_db_path(root, read_only=True)          # honours CRG_DATA_DIR, no mkdir
  lock    = WriteLock(db_path).acquire(blocking=wait_for_lock)
            # flock(LOCK_EX) on <data-dir>/write.lock, own PID into the file
            # not acquired -> {status:"contended", holder_pid} -> CLI exit 75
  _prepare_db_path(root, db_path)          # mkdir .code-review-graph/, migrate legacy .db
  store   = open_graph_store(recover_corrupt=True)      # corrupt -> discard + reopen, build only

  # ---- escalation ladder: any hit forces a FULL rebuild ----
  if !store.has_nodes():                        full_rebuild = True
  if read_build_state(store) == "in-progress":  full_rebuild = True
  if base is None: base = resolve_incremental_base(store)   # stored git_head_sha if commit exists
                   if base is None: full_rebuild = True
  if _should_rebuild_partial_graph(store):      full_rebuild = True
       # skip when intentionally_incomplete, svn, <8 files,
       # verified_file_count coverage > 25%, or tracked coverage >= 50%

  previous_state = metadata.build_state
  store.set_metadata(build_state, "in-progress")

  if full_rebuild: result = full_build(root, store, recurse_submodules, enrich, follow_symlinks)
  else:            result = incremental_update(root, store, base, ...)
  ...
  advance_to_postprocess_pending(store)
  warnings = _run_postprocess(store, postprocess, full_rebuild=...)
  if postprocess != "none" and not contended:
      store.set_metadata(build_state, "complete")
  store.close(); lock.release()
```

## 1.2 `full_build` — inventory → filter → parse → store

```
full_build(repo_root, store, recurse_submodules, enrich, follow_symlinks)
  repo_root = canonical(repo_root)
  parser    = CodeParser(repo_root)                    # one per process/worker (threading.local)

  files, followed, followed_dirs = _collect_files(repo_root, ...)
      patterns  = DEFAULT_IGNORE_PATTERNS + .code-review-graphignore
                  + nested_build_output_patterns(module-scan depth 3, <=2000 dirs, 300s TTL)
      candidates = git ls-files + untracked        # or svn list, or rglob(minus install_manifest)
      keep iff  !should_ignore(rel, patterns)
                and abs path <= 1000 chars, component <= 255 bytes
                and is_file() and not is_symlink()
                and detect_language(path) is not None
                and not is_binary(path)               # NUL byte in first 8192
      if follow_symlinks: add _walk_symlinked_dirs(...)   # loops skipped, cap 10000 files

  stale = _reconcile_stale_files(root, store, files)   # stored-but-now-unparseable -> remove
  _store_followed(store, followed...)                  # recorded BEFORE parsing

  if CRG_SERIAL_PARSE=1 or len(files) < 8:             # ---- serial path ----
      for rel in files:
          bytes = read(rel); fhash = sha256(bytes)
          nodes, edges = parser.parse_bytes(full_path, bytes)
          _relink_edges(edges, repo_root, followed_dirs)
          store.store_file_nodes_edges(full_path, nodes, edges, fhash)   # NOT wrapped in try
      # a parse error is recorded and the file keeps no rows;
      # a WRITE error propagates: no VCS anchor, next run rebuilds
  else:                                                # ---- parallel parse, serial store ----
      _prewarm_parser_probes(parser, files)
      with _make_executor(_MAX_PARSE_WORKERS) as ex:           # min(cpu,8); process unless MCP-stdio
          for (rel, nodes, edges, err, fhash) in ex.map(_parse_single_file, args, chunksize=20):
              if err: errors.append(...); continue
              _relink_edges(edges, repo_root, followed_dirs)
              store.store_file_nodes_edges(full_path, nodes, edges, fhash)   # 1 txn per file

  # ---- metadata ----
  set last_updated, last_build_type="full", intentionally_incomplete="0",
      verified_file_count = 0 if errors else count(files),
      cpp_identity_pending, nested_scope_versions, csharp_identity_pending_files

  # ---- resolvers: fixed order, each best-effort ----
  _run_python_resolver(store)          # python_resolver
  _run_rescript_resolver(store)        # rescript
  _run_spring_resolver(store)          # spring DI
  _run_aop_resolver(store)             # AOP advice
  _run_spring_event_resolver(store)    # events
  _run_temporal_resolver(store)        # temporal
  _run_hcl_resolver(store)             # terraform
  _run_scoped_resolver(store, root)    # rust/php/cs/tsconfig/npm-alias/workspace inside
  _run_jedi_enrichment(store, enrich)  # opt-in: CRG_ENABLE_ENRICHMENT or --enrich
  _relink_stored_edges(store, ...)
  _refresh_target_resolution(store)    # inherited endpoints -> unbound endpoints -> certainty cols
  set dependents_reparse_pending = "[]"

  advance_to_postprocess_pending(store)          # in-progress -> postprocess-pending
  _store_vcs_metadata(repo_root, store)          # git_branch, git_head_sha
  store.commit()
```

Node shape: `{kind File|Class|Function|Type|Test, name, file_path, line_start, line_end, language,
parent_name, params, return_type, modifiers, is_test, extra, identity_name}`.
Edge shape: `{kind CALLS|IMPORTS_FROM|INHERITS|IMPLEMENTS|CONTAINS|TESTED_BY|DEPENDS_ON|REFERENCES,
source, target, file_path, line, extra}`. Targets may be **bare** names at this
stage — that is what the resolvers exist to fix.

## 1.3 `_run_postprocess` — 11 ordered phases

```
_run_postprocess(store, postprocess ∈ {none, minimal, full}, full_rebuild,
                 changed_files, entry_point_qns, communities_were_affected)
  if postprocess == "none":
        store.refresh_target_resolution(); return                          # phase 0 only
  if !skip_resolvers and !(noop_incremental and no nodes without signature):
        resolve_bare_call_targets(); resolve_bare_tested_by_sources();
        resolve_cpp_scoped_call_targets(); refresh_target_resolution()      # 1,2
  update_node_signatures()              # only WHERE signature IS NULL      # 3
  if full_rebuild: rebuild_fts_index() else: update_fts_index(changed)     # 4  delta|rebuild
  refresh_embeddings()  if --embedding-provider AND --embedding-model      # 5  opt-in
  if postprocess == "minimal": return
  flows:   noop -> 0 ; incremental -> incremental_trace_flows
           else -> trace_flows() + store_flows()                          # 6
  communities: incremental_detect_communities()
           else detect_communities() [Leiden via igraph, file-based fallback] + store_communities()  # 7
  refresh_embeddings()  if opt-in                                         # 8
  set last_postprocessed_at, postprocess_level; shrink_memory()           # 9
```

`_run_postprocess` is **not** `postprocessing.run_post_processing` — the latter is the MCP/watch/forget
path and lacks the resolvers and the second `refresh_target_resolution`.

## 1.4 `incremental_update` — the delta path

```
incremental_update(repo_root, store, base, changed_files, reconcile_stale, ...)
  # -- identity gates that silently escalate to a full build --
  if cpp_identity_version != "1" and has_cpp_nodes and no pending: return full_build()
  pending_csharp = all .cs files   if csharp_identity_version != "5"
  pending_nested = files            if nested_scope version bumped

  # -- three independent change signals, unioned --
  content_mismatches, hashes, known_text = _find_content_mismatches(root, store)
        # EVERY indexed file re-read once; sha256 vs stored FileInfo.file_hash
  changed = dedupe( [*changed_files, *git_diff_paths(base), *content_mismatches,
                     *pending_csharp, *pending_nested, *followed_new] ) - followed_gone
  stale   = _reconcile_stale_files(root, store, known_text, remove=False)   # deferred removal

  if nothing changed and !stale and !identity_pending and !authoritative_git_sync:
        return {files_updated: 0}                       # no-op

  # -- dependents expansion --
  for f in changed:
        if existing nodes and hash unchanged and not pending: continue   # duplicate watch event
        dependent_files |= find_dependents(f)    # depth 2 (CRG_DEPENDENT_HOPS), cap 500 files

  # -- classify each file --
  for f in changed | dependents | identity_pending:
        if should_ignore(f): bookkeeping; continue
        if symlink or missing or outside-not-followed: missing.add(f); continue
        if detect_language(f) is None: continue
        if hash == stored_hash and not pending:
              if f not in dependent_files: continue          # untouched
              edges_only[f] = hash                           # dependents: edges rewritten only
        else: to_parse.append(f)

  # -- capture derived state before it is invalidated (#569) --
  lifecycle = expand_changed_file_paths(to_parse | missing | stale)
  community_by_qn = capture_community_assignments(store, lifecycle)
  entry_qns, flow_node_ids = capture_affected_flow_entry_points(...), capture_flow_node_ids(...)

  def store_parsed(f, nodes, edges, hash):
        _relink_edges(...)
        if edges_only.get(f) == hash: store.store_file_edges(f, edges)   # keeps node ids
        else:                         store.store_file_nodes_edges(f, nodes, edges, hash)

  parse serially or via executor.map(chunksize=20) exactly as full_build
  failures -> metadata.dependents_reparse_pending = failed dependents

  if to_remove = missing | stale:
        clear_flows_for_files(); restore_csharp_references()
        store.remove_files_permanently(to_remove, stored_paths=True)
  remap_community_assignments(); remap_flow_node_ids()
  purge_orphan_flow_data(); purge_empty_communities()
  communities_were_affected = community_by_qn and (parsed > 0 or to_remove)

  # -- resolvers are gated by candidate file extensions --
  run python|rescript|spring|aop|event|temporal|hcl|scoped when candidate_paths match
  jedi only for reparsed .py and only when enrichment requested
  _relink_stored_edges() / _refresh_target_resolution()  if there were graph changes

  if files_updated or authoritative_git_sync:
        advance_to_postprocess_pending(store); _store_vcs_metadata(); store.commit()
  if parsed_files >= 20: parser.clear_caches(release_memory = parsed_files >= 50)
```

`detect-changes` is the read-only sibling: `discover_review_changes` → `resolve_review_base`
(merge-base rule) → `get_changed_files` → `analyze_changes` risk score. No parse, no write.

## 1.5 CRG behaviour knobs

| Knob | Value | Effect |
|---|---|---|
| `_MAX_PARSE_WORKERS` | `CRG_PARSE_WORKERS` or `min(cpu, 8)` | pool size |
| executor kind | process; **thread** for MCP-stdio / win32 non-TTY | avoids pipe-inheritance deadlock |
| serial cutoff | `< 8` files or `CRG_SERIAL_PARSE=1` | no pool |
| `chunksize` | 20 | `executor.map` |
| FTS delta ceiling | `max(500, 0.35 × nodes)` | above it, full `rebuild` |
| dependent expansion | depth 2, cap 500 | sets `.truncated` |
| binary probe | NUL in first 8 KiB | skip |
| partial-graph rebuild | < 25 % verified / < 50 % tracked coverage | full rebuild |

---

# 2. `codebase-memory-mcp` — push-based, phase-barriered, staged publish

## 2.1 Trigger and admission

```
main() -> "cli" -> run_cli()
  if flag --index-worker: srv = mcp_server_new(); handle_tool(tool, args)  # the worker
  else:                   main_local_cli_daemon_execute(tool, args)         # the client

mcp_index_repository(args)
  mode = full | moderate | fast | cross-repo-intelligence
  if mode == cross-repo-intelligence: return            # no index job exists for this
  if index_supervisor_should_wrap():                    # host never indexes in-process
        return index_run_supervised(...)                # re-exec: cli --index-worker ... 
  if !mcp_project_mutation_begin(project): return "blocked by another mutation"
  p = cbm_pipeline_new(repo_path, db=NULL, mode)
  optional: set_project_name, set_resource_policy, set_publish_gate
  try_artifact_bootstrap(project, repo_path)            # .zst import if no local DB
  cbm_pipeline_lock()                                   # atomic per-process busy flag, 100 ms spin
  rc = cbm_pipeline_run(p)
  cbm_pipeline_unlock()
```

Three independent admission gates: the in-process busy flag, a **cross-process per-project lease**
(`cbm-project-v1:<name>`), and daemon job dedupe by project key. A worker crash/hang is retried with a
quarantine journal; a file is skipped only after appearing in **two consecutive** suspect sets.

## 2.2 Stage, then route

```
cbm_pipeline_run(p)
  final = resolve_db_path(p)                     # CBM_CACHE_DIR/<project>.db
  sweep_orphan_stages(final)                      # reclaim dead "<base>.stage.XXXXXX"
  stage = create_staging_path(final); p->db_path = stage
  rc = cbm_pipeline_run_staged(p)
  if rc != 0:              discard(stage); return rc         # live DB NEVER touched
  if cancelled or live_noop: discard(stage); return ABORT_PRESERVE_DB | finish_live_noop(final)
  seal(stage); before_publish_hook(); seal(stage)   # a test hook may re-enable WAL
  if test-declaration source drift or !publish_gate(): discard; return ABORT_PRESERVE_DB
  prepare_publish_destination(final, ...)           # existence flip -> abort
  rename(stage, final)  or rollback_quarantined_generation() + fail
  stage_owner_release(stage); export_after_publish(p, final)     # artifact is AFTER publish
```

```
cbm_pipeline_run_staged(p)
  mode = requested_mode; maybe promote_mode_to_existing_coverage(p)
  refresh_git_context(p); set_macro_extraction(mode == FULL)
  userconfig = load_test_config(p)                          # aborts on error (ABORT_PRESERVE_DB)

  # ---- Phase 1: discover ----
  rc = discover_ex2(repo_path, opts, &files, &count, &excluded, &ignored, ...)
  if rc != 0: return RESOURCE_LIMIT | NOT_FOUND              # no partial graph

  # ---- semantic manifest snapshot (drives no-op + publish-time re-validation) ----
  baseline = build_semantic_manifest(project, repo, files, count, git_ctx, userconfig)

  # ---- Phase 2: route ----
  rc = try_incremental_or_delete_db(p, files, count, baseline)
       # 0            -> live_noop or incremental published : done
       # FORCE_FULL   -> fall through, existing DB deleted
       # ABORT/PERSIST-> propagate

  gbuf = gbuf_new(project, repo_path); registry = registry_new()
  path_aliases = load_path_aliases(excluded_dirs)      # tsconfig/jsconfig, NULL if none
  ctx = {project, repo, gbuf, registry, mode, path_aliases, excluded_dirs, ...}

  run_extraction_phase(p, ctx, files, count)
  run_post_extraction(p, ctx, files, count, baseline)
```

## 2.3 Route: no-op / incremental / full

```
try_incremental_or_delete_db(p, files, count, baseline)
  if p->stage_backup_pending:
        if live_generation_current(p, baseline):     # read-only integrity + format + exact manifest
              p->live_noop = true; return 0          # nothing copied, sealed or renamed
        materialize_stage_backup(p)                  # copy live -> stage
  if !p->existing_generation: return FORCE_FULL_REINDEX
  if db invalid: return FORCE_FULL_REINDEX
  if format_version != CBM_INDEX_FORMAT_VERSION:
        adr_migrate_before_delete(db); unlink(db); remove_sidecars(db); return FORCE_FULL_REINDEX
  rc = run_incremental(p, db, files, count, baseline)
  if rc == FORCE_FULL_REINDEX: adr_migrate_before_delete(db); unlink(db); remove_sidecars(db)
  return rc

run_incremental:
  store = open_path_query(db)                # query_only: never re-WAL a sealed generation
  stored = get_file_hashes(project)          # sha256 + mtime_ns + size per file
  if exact_generation(p, store, stored, baseline):        # manifest set+bytes, coverage, doclinks
        return 0                                          # "incremental.noop"
  if !closure_plan(stored, baseline): return FORCE_FULL_REINDEX   # planner declines on doubt
  changed = closure_plan (or classify by mtime+size: legacy test path only)
  deleted = files present in stored but absent on disk (mode-filtered files are kept)
  if !closure and changed == 0 and deleted == 0: return 0
  old_cov = coverage_get(project)            # rows for files NOT re-extracted survive
  stage = cbm_delta_stage_clone(db)          # copy-on-write clone of the live generation
  snapshot inbound cross-file edges into changed files
  upsert File nodes for changed paths
  parallel_extract(changed) -> registry -> lsp surfaces -> cross registries -> parallel_resolve
  pass_k8s(changed)
  postpasses(changed): tests -> decorator_tags -> configlink -> operator -> doclinks
                       -> similarity (if mode <= MODERATE) -> semantic_edges
                       # importance is NOT run here; rescored in SQL after the patch
  if image components differ: goto FORCE_FULL
  merge doc-link rows; drop BUILD/ENVJOIN edges wholesale
  delta_patch(stage, gbuf, max_db_id, snapshot)      # one transaction
  rescore importance; drop edgeless; drop unreferenced
  publish_staged(stage, fts_wholesale=false, known_healthy=true)
```

## 2.4 Full path: extraction

```
run_extraction_phase(p, ctx, files, count)
  pass_structure(p, stem_marks, files, count)             # B1: Project, Branch, Folder/Package,
                                                          #     File nodes + CONTAINS_FILE
  workers = frozen ? 1 : cbm_pipeline_worker_count(p)
  if workers > 1 and count > MIN_FILES_FOR_PARALLEL(=50): parallel_extract(...)   # P2
  else:                                                   run_sequential_pipeline(...)
  gbuf_delete_unreferenced(gbuf, "Image", DK_IMAGE_EDGES)

# ---- parallel (P2): extract_worker per file, work-stealing, main thread participates ----
parallel_extract_ex(ctx, files, count, workers):
  slab_install(); sort files by size DESC                  # tail-latency
  shared_ids = atomic gbuf next_id                         # ONE id space for all workers
  parallel_for(workers, extract_worker, {...}):
      loop:
        back-pressure gate (mem over budget and cycle still over) 
        fi = files[atomic_fetch_add(next_file_idx)]
        if quarantined(fi): pp_err_add("crash"|"hang"|"error"); continue
        source = read(fi)                                  # 512 MiB cap -> CBM_READ_OVERSIZED
        res = cbm_extract_file_ex_with_tests(source, lang, project, rel,
                                             budget = 5 s, macro_table, test_declarations)
        if !res: pp_err_add("extract failed"); continue
        if res->has_error:      pp_err_add("extract", msg)
        elif parse_incomplete:  pp_err_add("parse_partial" | "parse_unusable", ranges)
        if walk_truncated:      pp_err_add("walk_truncated", "N/M")
        for d in res->defs: insert_def_into_gbuf(worker_gbuf, stem_marks, fi, d)
              # props: "fp" MinHash hex, "sp" AST profile, decorators, complexity
              # id = atomic_fetch_add(shared_ids, 1)
        pkgmap_try_parse(basename, source)                 # package manifests
        res = compact(res); spill_active ? park(res) : result_cache[idx] = res
        destroy_thread_parser(); slab_reclaim(); mem_collect()     # per-file RSS bound
  merge every worker's local gbuf into ctx->gbuf; merge pkg entries; merge error lists
  over_budget && memory futile -> ABORT_OVER_BUDGET

# ---- registry (B3) then cross-LSP (B4) then fused resolve (P6) ----
build_registry_from_cache(ctx)      # QN + simple-name registry, import-distance scoring
lsp_cross_prepare(ctx)              # all_defs, lsp_surfaces rows, module->defs index
                                    #   (CBM_DISABLE_LSP_CROSS opts out)
parallel_resolve(ctx)               # imports, CALLS, USAGE/THROWS/READS/WRITES, SPAWNS,
                                    # HTTP_CALLS, ASYNC_CALLS, doc refs; unresolved recorded

# ---- sequential (<50 files): different phase order, same intent ----
seq_passes = [ definitions, k8s, lsp_cross, calls, usages, semantic,
               doc_links ]                     # NOTE: k8s BEFORE calls here
then infra routes/bindings, then the same tail
```

## 2.5 Full path: the post-extraction pass graph

Barriers are real: B = single-threaded ordered boundary, P = internally parallel, S = serial loop.

| # | Pass | Gate | Writes |
|---|---|---|---|
| 1 | `pass_structure` | all | `Project`, `Branch`+`HAS_BRANCH`, `Folder`/`Package`, `File`+`CONTAINS_FILE` |
| 2 | `parallel_extract_ex` | all | definitions, `DEFINES`, `fp`/`sp`/`bt` props |
| 3 | `build_registry_from_cache` | all | symbol registry, `DEFINES_METHOD` |
| 4 | `lsp_cross_prepare` | unless `CBM_DISABLE_LSP_CROSS` | `lsp_surface` rows, module→defs index |
| 5 | `doclinks_begin` | all | doc-link indexes |
| 6 | `parallel_resolve` (fused) | all | `IMPORTS`, `CALLS`, `USAGE`, `SPAWNS`, `HTTP_CALLS`, `ASYNC_CALLS`, `REFERENCES` |
| 7 | `extract_infra_routes` + `process_infra_bindings` | all | `Route` nodes, `INFRA_MAPS` |
| 8 | `pass_k8s` (+ shell, terraform, ansible, docker, env_join) | all | `Resource`, `Chart`, `Image`, `DEPLOYS`, `CONFIGURES` |
| 9 | `pass_tests` | all | `TESTS`, `TESTS_FILE` |
| 10 | `run_githistory` | `mode != FAST` | `CO_CHANGED` |
| 11 | `predump_deco` | all | decorator tag props |
| 12 | `predump_cfg` (`configlink`) | all | `CONFIGURES` |
| 13 | `predump_operator` | all | `HANDLES`, `USAGE {ref:crd-type}`, `REFERENCES_FILE` |
| 14 | `predump_doclinks` | all | `REFERENCES_FILE` |
| 15 | `predump_route` | all | `Route` nodes, re-targeted edges, `HANDLES` |
| 16 | `predump_ensemble` | all | ensemble routing edges |
| 17 | `predump_sim` (similarity) | `!FAST` | `SIMILAR_TO` (MinHash Jaccard ≥ 0.95, ≤10/node) |
| 18 | `predump_sem` (semantic_edges) | `!FAST` or doc-candidates | `SEMANTICALLY_RELATED`, vector rows |
| 19 | `predump_complexity` | all | `transitive_loop_depth`, `recursive` |
| 20 | `predump_importance` | all, **must be last** | `importance` prop |
| 21 | `dump_and_persist_hashes` | — | the stage DB |

`importance` is last by construction: it reads `CALLS`/`USAGE` (extraction) and `TESTS` (pass 9).

## 2.6 Dump and publish

```
dump_and_persist_hashes(p, baseline)
  gbuf_settle_variants(gbuf)
  temp_to_final[] = build_dump_nodes(gbuf)     # skip deleted, assign final rowids
  release_gbuf_indexes(gbuf)                   # ~3.8 GB at kernel scale, freed BEFORE edges
  w = writer_open(stage)                       # raw page writer, page size 65536
  for partition of 65536 nodes: writer_append_nodes(w, chunk)
  edges = build_dump_edges(gbuf, temp_to_final)
  relocate vectors -> new ids
  writer_finalize(w, project, root, indexed_at, nodes, edges, vectors, token_vectors)
        # writes tables, then PARALLEL_SORT_INDEXES and builds all 11 index B-trees,
        # then the sqlite_master page — the whole file is authored, no CREATE INDEX

publish_staged(stage, fts_wholesale)
  store = open_path(stage)
  replace file_hashes(project, baseline)         # the semantic manifest
  unless in_place: replace lsp_surfaces / doc_candidates
  set_format_version(CBM_INDEX_FORMAT_VERSION)
  coverage_replace_ex(project, coverage, meta)   # index_coverage + kinds + recording_status
  publish_doc_links(store)  or write the "doc-link layer failed" marker
  if fts_wholesale: fts_rebuild(store)           # delete-all-and-rebuild
  generation_advance(store)                      # new uid / mutation counter
  check_integrity(store)                         # quick_check + root_path sanity
  seal_for_atomic_publish(store)                 # synchronous=FULL, WAL checkpoint TRUNCATE,
                                                 # journal_mode=DELETE
  if !ok: discard(stage); return PERSIST_FAILED
  finalize_staged(stage, final):
        remove sidecars(stage)                   # -wal/-shm/-journal
        prepare_existing_generation_for_replace(final)   # quarantine, not delete
        rename(stage, final)  else rollback_quarantined_generation()
  export_after_publish(): artifact_export(final, repo, BEST|FAST)     # AFTER publish, never gates it
```

## 2.7 Incremental freshness detection

- Pipeline routing: **SHA-256 semantic manifest equality** (`exact_generation`). Never mtime.
- mtime+size is used only by the legacy test-only classifier.
- Watcher: git **HEAD hash** + a folded **dirty signature** (porcelain entries with per-entry
  size/mtime); non-git roots use a tree signature over its own discovery walk.
- Poll: `5 s + 1 s per 500 files`, capped at 60 s; success cooldown `max(interval, 3 × elapsed)`
  capped 300 s; failure backoff `interval << failures` capped 300 s.

## 2.8 Guards

| Guard | Value |
|---|---|
| `index_max_files`, `index_max_source_mb` | off by default; breach → no graph published |
| `index_max_rss_mb`, `index_max_duration_seconds` | off; supervisor kills the worker, publication fenced |
| single source file | 512 MiB |
| per-file parse budget | 5 s |
| worker quiet timeout | 15 min without a new log line |
| artifact decompressed ceiling | 64 GiB |

Breach semantics: the stage is discarded and the previous generation keeps serving. There is **no
path** that publishes a partial graph.

---

# 3. Divergence that matters

| | `code-review-graph` | `codebase-memory-mcp` |
|---|---|---|
| Write model | in place, 1 txn per file, `flock` + `BEGIN IMMEDIATE` | stage → seal → `rename()`; live DB untouched until the rename |
| Driver | pull: caller asks for build/update | push: supervisor workers, daemon job queue, watcher |
| Parallelism | parallel **parse** (process/thread pool), serial store | parallel extract **and** parallel resolve; atomic shared id space; main thread participates |
| Phase graph | linear list, resolvers gated by candidate extensions | 21 phases, explicit barriers, mode gates (`FAST` skips similarity/semantic) |
| Failure isolation | per-file try/except; write error aborts the build (no anchor) | per-file error recorded in `index_coverage`; a whole-run abort preserves the old DB |
| No-op detection | three signals; `files_updated == 0` short-circuit | exact manifest equality → `live_noop`, stage never even materialised |
| Incremental unit | file (+ dependents, depth 2) + derived-table deltas for flows/communities/FTS | file → closure plan, patched into a **clone** of the published DB |
| Dump | SQL `INSERT … ON CONFLICT` | hand-authored SQLite pages (page size 65536, indexes sorted in parallel) |
| Recovery | `build_state` tri-state decides full vs `postprocess` | stage sweep + quarantine journal + supervisor retry |

## Doc/code contradictions found (code authoritative)

1. The pipeline's own file header claims a "bulk load sources with LZ4 HC" phase and a
   "communities" phase — neither exists. LZ4 is used by the Aho-Corasick matcher's fused
   decompression, not by the pipeline.
2. The project's own documentation claims "in-memory SQLite, single dump at end" — the dump writes
   a **file** via the raw page writer; `open_memory()` is only the watcher's bookkeeping store.
3. The documented phase order is wrong: k8s/infra runs **before** calls on the sequential path,
   tests before configlink, route nodes after doclinks.
4. `cbm_pipeline_pass_githistory` is declared and defined with **no caller**; the real phase is
   `run_githistory`.
5. `postprocessing.run_post_processing` is not the build pipeline despite its docstring: it claims
   five steps and omits the resolvers and the second `refresh_target_resolution`.
6. The SQLite writer's file header says page size 4096; the actual definition is
   `CBM_PAGE_SIZE` = 65536.
