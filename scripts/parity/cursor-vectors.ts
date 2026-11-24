#!/usr/bin/env bun
/**
 * Mints the six cross-implementation cursor vectors (`RUST-MIGRATION.md` §7 phase 0, §8 R4; plan
 * `rust-migration` TASK-004).
 *
 * The two implementations serve the same graph during the migration, so a token minted by
 * `src/shared/cursor.ts` must verify in `sangkar-core` and vice versa. The format is small enough
 * to pin byte for byte — base64url without padding, compact JSON in a fixed key order, a 16-byte
 * HMAC-SHA256 tag over the *encoded* body — but every one of those three is a place where two
 * runtimes disagree silently. This script is the oracle: it prints `JSON.stringify`-ed vectors that
 * `rust/crates/sangkar-core/tests/cursor_interop.rs` re-mints and re-verifies exactly.
 *
 * Usage, from the repository root:
 *
 *     ~/.bun/bin/bun run scripts/parity/cursor-vectors.ts > rust/tests/vectors/cursor_vectors.json
 *
 * The key below is fixed, 32 bytes of ASCII, and deliberately not a secret: both sides must sign
 * with the same bytes for the tokens to be comparable. `rust/tests/parity/README.md`'s capture key
 * is a different one, and neither is ever a real deployment key.
 *
 * The six payloads cover what actually varies, not what would be convenient:
 *
 * - the three `argsHash` **shapes** the handlers really produce, since the inconsistency is real
 *   and is being reproduced rather than fixed: `sha256:<64 hex>` (`trace`, `expand`, `status`,
 *   `summary`), bare 64 hex (`search`), and a 32-character truncation (`impact`). The per-tool
 *   builders stay in the handlers (TASK-027..TASK-033) and are pinned by the golden harness
 *   (TASK-036), which compares the `next` token; this script only needs the shapes.
 * - both repo states: `""` for the unbound facade and an absolute path for the daemon's
 *   per-repository binding.
 * - both boundary positions: `0`, the first continuation a page boundary can address, and `37`, an
 *   ordinary mid-list offset.
 */
import { createHash } from "node:crypto";
import { sign } from "#shared/cursor";

/** The key both implementations sign with. 32 ASCII bytes, exactly the floor `CURSOR_KEY_MIN_BYTES` sets. */
const KEY = "0123456789abcdef0123456789abcdef";

/** An absolute POSIX path, the shape `canonicalRoot(repoRoot)` binds a facade to. Never a real one. */
const REPO = "/srv/repos/sangkar-fixture";

/** A `sha256` hex digest of a literal, so every `argsHash` below is well-formed by construction. */
function digestOf(seed: string): string {
  return createHash("sha256").update(seed, "utf8").digest("hex");
}

interface Vector {
  /** What this vector is here to cover, spelled for a failing assertion's message. */
  readonly label: string;
  readonly generation: string;
  readonly tool: string;
  readonly argsHash: string;
  readonly position: number;
  /** The `repo` stamp: `""` is the unbound facade, otherwise the repository the page belongs to. */
  readonly repo: string;
  /** What `sign(payload, KEY, repo)` returns — the bytes the Rust side must reproduce. */
  readonly token: string;
}

const PAYLOADS = [
  {
    label: "trace: sha256:<64 hex>, unbound repo, position 0",
    generation: "g-7f3a91",
    tool: "trace",
    argsHash: `sha256:${digestOf("trace:alpha")}`,
    position: 0,
    repo: "",
  },
  {
    label: "expand: sha256:<64 hex>, bound repo, position 37",
    generation: "g-7f3a91",
    tool: "expand",
    argsHash: `sha256:${digestOf("expand:src/index.ts")}`,
    position: 37,
    repo: REPO,
  },
  {
    label: "search: bare 64 hex, bound repo, position 37",
    generation: "g-7f3a91",
    tool: "search",
    argsHash: digestOf("search:alpha:null:"),
    position: 37,
    repo: REPO,
  },
  {
    label: "search: bare 64 hex, unbound repo, position 0",
    generation: "g-91c4de",
    tool: "search",
    argsHash: digestOf("search:beta:function:src"),
    position: 0,
    repo: "",
  },
  {
    label: "impact: 32-char truncation, unbound repo, position 0",
    generation: "g-91c4de",
    tool: "impact",
    argsHash: digestOf("impact:src/a.ts").slice(0, 32),
    position: 0,
    repo: "",
  },
  {
    label: "impact: 32-char truncation, bound repo, position 37",
    generation: "g-91c4de",
    tool: "impact",
    argsHash: digestOf("impact:src/b.ts").slice(0, 32),
    position: 37,
    repo: REPO,
  },
] as const;

const vectors: Vector[] = PAYLOADS.map((payload) => ({
  ...payload,
  token: sign(
    {
      generation: payload.generation,
      tool: payload.tool,
      argsHash: payload.argsHash,
      position: payload.position,
    },
    KEY,
    payload.repo,
  ),
}));

process.stdout.write(`${JSON.stringify(vectors, null, 2)}\n`);
