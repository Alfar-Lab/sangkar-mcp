/**
 * The runtime preflight. This product runs on `bun:sqlite`, `Bun.spawn` and `bun build`, so a
 * foreign or stale interpreter has to be refused before anything opens a database. `engines.bun`
 * in `package.json` states the same floor, but an install-time check never sees a stale global
 * install or a copied binary — this module is the check that actually runs.
 */

export const MIN_BUN = "1.4.4";

/** EX_SOFTWARE: a setup fault, distinguishable from a crash (1) and a usage error (2). */
export const RUNTIME_EXIT = 70;

export interface RuntimeProbe {
  readonly bun: string | undefined;
  readonly node: string | undefined;
}

/** Bun ships nightlies as `1.4.4-canary.1`; the numeric prefix is what decides the floor. */
function segment(part: string | undefined): number {
  const parsed = Number.parseInt(part ?? "0", 10);
  return Number.isNaN(parsed) ? 0 : parsed;
}

function atLeast(found: string, minimum: string): boolean {
  const left = found.split(".");
  const right = minimum.split(".");
  const width = Math.max(left.length, right.length);
  for (let index = 0; index < width; index += 1) {
    const a = segment(left[index]);
    const b = segment(right[index]);
    if (a !== b) {
      return a > b;
    }
  }
  return true;
}

/**
 * The interpreter this process actually runs on. `process.version` is kept verbatim (node's `v`
 * prefix included): the diagnostic should report what the runtime says about itself, not a
 * cleaned-up rewrite of it.
 */
export function readRuntime(): RuntimeProbe {
  return {
    bun: typeof Bun === "undefined" ? undefined : Bun.version,
    node: process.version,
  };
}

function resolvedBinary(): string {
  // `which` is null whenever bun is installed but not on PATH — this machine's own default — so
  // the interpreter's path is the answer of last resort.
  const onPath = typeof Bun === "undefined" ? null : Bun.which("bun");
  return onPath ?? process.execPath;
}

/** `null` means "this runtime is supported"; otherwise the one line to report and exit on. */
export function runtimeDiagnostic(runtime: RuntimeProbe): string | null {
  if (runtime.bun !== undefined && atLeast(runtime.bun, MIN_BUN)) {
    return null;
  }
  const required = `Bun >= ${MIN_BUN}`;
  return JSON.stringify({
    level: "error",
    msg: "runtime.unsupported",
    required,
    found: runtime.bun ?? runtime.node ?? "unknown",
    resolved: runtime.bun === undefined ? "not bun" : resolvedBinary(),
    hint: `install ${required} and re-run`,
  });
}

/**
 * Writes the diagnostic to stderr and returns the exit code the caller must use. stdout is never
 * written, not even as a newline: it carries the MCP protocol, and a refusal has to leave it
 * byte-empty (TECH-SPEC.md §11) or a client reads a truncated greeting. The sink is a parameter so
 * a test can prove both halves without patching a global.
 */
export function guardRuntime(
  runtime: RuntimeProbe,
  writeStderr: (text: string) => void = (text) => {
    process.stderr.write(text);
  },
): number | null {
  const diagnostic = runtimeDiagnostic(runtime);
  if (diagnostic === null) {
    return null;
  }
  writeStderr(`${diagnostic}\n`);
  return RUNTIME_EXIT;
}
