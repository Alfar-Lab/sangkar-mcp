# Risks and open questions

## Risks

| Risk | Why it is real | Mitigation | If it fires |
|---|---|---|---|
| **R1 TOON encoder differs** | `serde_toon_format` 0.2.0 implements the **v4.1 working draft**; the pin is `@toon-format/toon` **4.4.0**, whose README says key-folding/flatten-depth "were removed by spec 4" | TASK-009 spikes ten real envelopes through the Rust encoder and the TS `decode()`, asserting deep equality — including the `tools/call` text | Vendor the TS encoder (61.7 KB dist, ~1–2 k lines) as `sangkar-toon`; the renderer task then depends on that instead |
| **R2 `rmcp` framing or handshake** | The TS SDK is newline-delimited JSON-RPC with no `Content-Length`; a header-framed transport hangs the first client, and the suite pins the wire (`tests/mcp-surface.test.ts`) | TASK-010 records `initialize` → `tools/list` → `tools/call` from the TS server and replays it against the Rust one, diffing bytes | Hand-rolled JSON-RPC over `tokio` stdin/stdout lines (~400 lines) |
| **R3 grammar node-type drift** | The 12 rows were probed against `tree-sitter-wasms` 0.1.13; the native crates are independent releases (`tree-sitter-java` 0.23.5, `tree-sitter-c-sharp` 0.24.0, `tree-sitter-php` 0.25.2). `tests/lang.test.ts` exists because a missing name is silent | TASK-038 ports `lang.test.ts` first: every declared type probed against the loaded grammar and a parsed snippet, per language; TASK-048 compares the per-language census and the six ordered row sequences A/B | Record the substitution in the row and the measured extraction delta in `.agents/prompts/gaps-n-future-works.md` — a measured gap, never a silent one |
| **R4 cursor incompatibility** | The MAC is over the JSON *text*: key order, integer formatting and base64url padding all matter, and `argsHash` already differs per tool (`sha256:<hex>` vs bare hex vs 32-char truncation). Cross-process resumption only holds when `SANGKAR_CURSOR_KEY` is set — otherwise each process mints its own key, as today | TASK-004 fixed vectors both directions, committed as `rust/tests/vectors/cursor_vectors.json`; TASK-036 compares the `next` token byte for byte, which is what forces every per-tool `argsHash` to be right | None expected — the format is fully specified; the risk is schedule, not feasibility |
| **R5 SQLite edge behaviour** | FTS5 tokenizer args, `VACUUM INTO` on a read-only handle, the `journal_mode` switch while statements are cached, `INSERT … RETURNING`, integer affinity | TASK-006/007/008 run against real TypeScript-built databases, including a seeded stage and a sealed publish | Set the pragmas by hand at open time instead of reading them back; keep the `unsupported_filesystem` refusal |
| **R6 the `index` hand-over is forgotten** | Phase 2 ships a read-only binary whose `index` tool must still index, and Phase 3 changes who indexes | `AppConfig.worker_cmd` (default `[current_exe]`), overridden during the transition to the Bun worker; asserted by an `index` integration test in both phases (TASK-032, TASK-046) | Register `index` only from Phase 3; Phase 2's binary refuses it with a clear envelope instead of half-working |
| **R7 packaging on Linux / old glibc** | A binary built on glibc 2.43 (this machine) will not run on a 2.35 userland, and a shim that installs with no binary is worse than one that refuses. macOS/Windows are out of scope for the first release (open question 3, answered), so the `.cmd`-wrapper problem is deferred rather than solved | TASK-058 ships a `#!/usr/bin/env node` shim that names the platform it wanted, the single published platform and the version to pin; TASK-059 builds on the oldest supported image and records its glibc floor | Extend `optionalDependencies` + `npm/<key>/` + one release-matrix row per new platform; the shim needs no rewrite |
| **R8 scope drift into a redesign** | 25 773 lines with 31 338 lines of contract tests is exactly where "while I'm here" happens | Parity bar + forbidden-document rule in `plan.constraint_invariant.md`; a parity task closes every phase | Stop, report, and re-plan the drift as its own plan |
| **R9 effort underestimated** | 12 grammars, a daemon, two surfaces, and a test suite that is itself a specification | Phase 0 delivers a usable artifact; Phases 2 and 3 are viable cut points (see `RUST-MIGRATION.md` §7) | Cut after Phase 2 (read-only Rust server) or Phase 3 (Rust indexer + TS front end) and keep the TypeScript surface |
| **R10 the TypeScript suite rots while diverging** | It stays the oracle until Phase 5, but `src/` may keep changing for bug fixes during the migration | No task edits `src/**` except Phase 5's deletion; a TypeScript-side fix during the migration needs its own plan and re-captures goldens | Re-capture goldens from the new TypeScript HEAD and note it in the commit body |

## Open questions

**Answered by the maintainer on 2026-11-24** (recorded in `docs/my-product/RUST-MIGRATION.md` §1 and wired
into the tasks named below):

1. *Does the migration start now?* — **No. It starts later.** The plan is complete and idle; when it
   begins, refresh `plan.md`'s `Base commit` and the inventory numbers if `src/` moved.
2. *Does `0.6.0-beta.1` stay the published line while it runs?* — **Yes.** Nothing ships from this plan
   until the Phase 5 gate is green (TASK-068).
3. *Are Windows and macOS first-class?* — **No. The first Rust release is Linux x64 only** (`TASK-058`,
   `TASK-059`, `TASK-060`, `TASK-065`). macOS and Windows stay on the last TypeScript release; the shim
   names the platform it wanted and the version to pin.
4. *Does the package keep its name?* — **Yes, `sangkar-mcp`.** No `-rust` prerelease, no engine switch
   (TASK-058).

Still open:

5. **Which version does cutover publish?** *Not blocking:* the bump is decided at release time by the
   `build-n-deploy` skill. The runtime changes, the CLI and protocol do not, so the choice is between a
   minor and a major; the plan does not fix it. **The maintainer answers at Phase 5.**
6. **What is the glibc floor?** *Blocking for TASK-059 only:* the release image decides it (Ubuntu 22.04
   ⇒ glibc 2.35). Take that floor and publish it in the README rather than guessing. **The maintainer
   confirms the oldest image they are willing to support.**
