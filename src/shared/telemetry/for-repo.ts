/**
 * The one decision "record or not, and where", per repository.
 *
 * A daemon serves several trees from one process, so the recorder cannot be built from the
 * machine config alone: the consent switch is machine-level, but the directory, the root and the
 * repo's own `telemetry` flag belong to the tree. Keeping the precedence here — a call to
 * `recordingEnabled` plus the `createTelemetry`/`nullTelemetry` fork — means the composition root
 * never re-derives it, and the rule itself has exactly one owner in `#shared/repo-config`.
 *
 * There is no environment read: `config.telemetry.enabled` is the consent already resolved by
 * `#shared/config`, the only reader of the environment.
 */
import { join } from "node:path";
import type { AppConfig } from "#shared/config";
import { type RepoConfig, recordingEnabled } from "#shared/repo-config";
import type { SessionMode } from "#shared/telemetry/record";
import { createTelemetry, nullTelemetry, type Telemetry } from "#shared/telemetry/telemetry";

export interface RepoTelemetryInput {
  /** Machine-level: consent lives in `config.telemetry.enabled`. */
  readonly config: AppConfig;
  readonly repo: RepoConfig;
  /** Registration is part of the precedence, so an unregistered tree is never recorded. */
  readonly registered: boolean;
  readonly sessionMode: SessionMode;
  readonly serverVersion: string;
  readonly graphSchemaVersion: number;
  readonly now?: () => Date;
}

/**
 * A recorder for `input.repo`, or the same interface with no destination when the precedence says
 * no. The directory is the repo's own `dataDir` — not `config.telemetry.dir`, which a daemon that
 * swapped trees through `withRepo` would have pointed at the wrong one.
 */
export function telemetryForRepo(input: RepoTelemetryInput): Telemetry {
  const recording = recordingEnabled({
    consent: input.config.telemetry.enabled,
    repo: input.repo,
    registered: input.registered,
  });
  if (!recording) {
    return nullTelemetry();
  }

  return createTelemetry({
    dir: join(input.repo.dataDir, "telemetry"),
    repoRoot: input.repo.repoRoot,
    dataDir: input.repo.dataDir,
    sessionMode: input.sessionMode,
    serverVersion: input.serverVersion,
    graphSchemaVersion: input.graphSchemaVersion,
    // `exactOptionalPropertyTypes` is on: an absent clock must stay absent, not be `undefined`.
    ...(input.now === undefined ? {} : { now: input.now }),
  });
}
