/**
 * `sangkar telemetry`: one verb, one file, one line of stdout.
 *
 * Everything but the seams lives elsewhere — `report.ts` reads and folds, `html.ts` renders — so
 * this module only resolves the flags, names the output, and decides which of the three exits the
 * run takes. There is no graph, no reader and no store here: a report is built from the telemetry
 * directory alone, so it answers on a tree that was never indexed.
 *
 * Synchronous IO on purpose. The file is one document written once, so a partial write is the only
 * failure that matters and a promise buys nothing; the `async` signature exists only because `main`
 * awaits every verb alike.
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { EXIT_FAILURE, EXIT_USAGE } from "#shared/cli/render";
import { flagValue, loadConfig } from "#shared/config";
import { renderHtml } from "./html";
import { aggregate, isSinceLiteral, parseSince, readRecords } from "./report";

/**
 * Printed on `--help` and after every refusal. There is no `--json`: the answer of this verb is a
 * file, and a machine-readable copy of it would be a second report with its own contract.
 */
const USAGE = `usage: sangkar telemetry [--out <path>] [--since <YYYY-MM-DD | Nd>] [--repo-root <path>] [--data-dir <path>]
  --out <path>        where the HTML report is written (default: <data-dir>/telemetry/report.html)
  --since <when>      include records from that day (YYYY-MM-DD) or the last N days (Nd); default: all
  --repo-root <path>  the tree the report describes (default: autodetected from cwd)
  --data-dir <path>   where the graph and the telemetry log live (default: <repo-root>/.sangkar)`;

export async function runTelemetry(argv: readonly string[]): Promise<number> {
  // Help is answered before any flag is validated, like every other verb: a caller asking for the
  // shape of the command must not have to satisfy it first.
  if (argv.includes("--help") || argv.includes("-h")) {
    process.stdout.write(`${USAGE}\n`);
    return 0;
  }

  // `flagValue` cannot tell a trailing `--since` (no value) from an absent one, so the bare token
  // is checked separately; `--since=` hands over an empty string, which the shape test rejects.
  const rawSince = flagValue(argv, "since");
  if (rawSince === undefined) {
    if (argv.includes("--since")) {
      return refuse("--since needs a value");
    }
  } else if (!isSinceLiteral(rawSince)) {
    return refuse(`--since must be YYYY-MM-DD or <n>d, not "${rawSince}"`);
  }

  const rawOut = flagValue(argv, "out");
  if (rawOut === undefined ? argv.includes("--out") : rawOut === "") {
    return refuse("--out needs a path");
  }

  const config = loadConfig({ argv });
  // Same rule as `--data-dir`: a relative path is the caller's spelling of "inside this tree", so
  // it resolves against `repoRoot` rather than against the process cwd.
  const out =
    rawOut === undefined
      ? join(config.telemetry.dir, "report.html")
      : resolve(config.repoRoot, rawOut);

  const now = new Date();
  const read = readRecords(config.telemetry.dir, { since: parseSince(rawSince, now) });
  const since = rawSince ?? null;
  const data = aggregate({
    records: read.records,
    unparsed: read.unparsed,
    files: read.files,
    since,
  });
  // One clock reading, taken before the write, so the two renders of one directory differ in this
  // field alone and the renderer stays a pure function of its arguments.
  const html = renderHtml(data, { generated_at: now.toISOString(), since });

  try {
    mkdirSync(dirname(out), { recursive: true });
    writeFileSync(out, html);
  } catch (error) {
    process.stderr.write(`sangkar telemetry: could not write ${out}: ${messageOf(error)}\n`);
    return EXIT_FAILURE;
  }

  process.stdout.write(
    `wrote ${out} (${data.totals.records} records, ${read.unparsed} unparsed lines)\n`,
  );
  return 0;
}

/** The reason first, then the block, so the last thing a caller sees is how to call it right. */
function refuse(reason: string): number {
  process.stderr.write(`${reason}\n${USAGE}\n`);
  return EXIT_USAGE;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
