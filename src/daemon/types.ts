/**
 * The daemon's vocabulary: what one process serving several repositories reports, and nothing
 * that reads or writes. Types only, so a test can build a `DaemonStatus` by hand.
 *
 * `src/daemon/**` may import only `#shared/*`, `src/indexer/**` and `./` siblings inside
 * `src/daemon`, never `src/tools/**` — the `daemon-privileges` rule in
 * `tests/boundary.test.ts`. Nothing here imports the indexer either: a status answer is plain
 * data, and a type that reached into `src/indexer/**` would tie the shape to the writer.
 */

/** One registered repository as the daemon sees it: where it is, and what it is doing there. */
export interface RepoRuntime {
  readonly root: string;
  /** False when the directory has gone since it was registered, so the daemon reports it instead. */
  readonly exists: boolean;
  /** Whether this repo has a live watcher, which is a per-repo decision, not a global one. */
  readonly watch: boolean;
  readonly watchIntervalMs: number;
  readonly telemetry: boolean;
}

/** The whole answer to "is the daemon up, and what is it serving". */
export interface DaemonStatus {
  /** `null` before the process has bound one, so a status read never needs the pid to exist. */
  readonly pid: number | null;
  /** ISO-8601, from the daemon's own clock; `null` until it has started serving. */
  readonly startedAt: string | null;
  readonly home: string;
  readonly registryPath: string;
  readonly repos: readonly RepoRuntime[];
}
