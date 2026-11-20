/**
 * `sangkar telemetry`: one verb, one file, one line of stdout — or, with `--serve`, one listener.
 *
 * Everything but the seams lives elsewhere — `report.ts` reads and folds, `html.ts` renders,
 * `serve.ts` binds and answers — so this module only resolves the flags, names the output, and
 * decides which of the exits the run takes. There is no graph, no reader and no store here: a report
 * is built from the telemetry directory alone, so it answers on a tree that was never indexed.
 *
 * Synchronous IO on the file path on purpose. The file is one document written once, so a partial
 * write is the only failure that matters and a promise buys nothing; the `async` signature exists
 * because the serve path really does await a listener and a signal.
 */
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { CliUsageError } from "#shared/cli/args";
import { EXIT_FAILURE, EXIT_USAGE } from "#shared/cli/render";
import { flagValue, loadConfig } from "#shared/config";
import { renderHtml } from "./html";
import { aggregate, isSinceLiteral, parseSince, readRecords } from "./report";
import { type ReportServer, serveReport } from "./serve";

/**
 * Printed on `--help` and after every refusal. There is no `--json`: the answer of this verb is a
 * file (or a page a browser reads), and a machine-readable copy of it would be a second report with
 * its own contract.
 */
const USAGE = `usage: sangkar telemetry [--out <path>] [--serve] [--port <n>] [--host <addr>] [--since <YYYY-MM-DD | Nd>] [--repo-root <path>] [--data-dir <path>]
  --out <path>        where the HTML report is written (default: <data-dir>/telemetry/report.html)
  --serve             serve the report for every registered repository on loopback instead of writing one file
  --port <n>          the port to bind (default: 7317; 0 picks a free port)
  --host <addr>       the address to bind (default: 127.0.0.1; loopback only)
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

  // The two answers are exclusive: one run either writes a file or answers on a socket, and a
  // caller that named both has not decided which one it wants.
  const serving = argv.includes("--serve");
  if (serving && rawOut !== undefined) {
    return refuse("--serve answers over http, so it takes no --out; drop one of the two");
  }

  // `flagValue` hands back the next token whatever it is, so the bare form and the shape are both
  // checked here; an empty `--port=` fails the shape test the same way a trailing flag does.
  const rawPort = flagValue(argv, "port");
  if (rawPort === undefined) {
    if (argv.includes("--port")) {
      return refuse("--port needs a value");
    }
  } else if (!isPortLiteral(rawPort)) {
    return refuse(`--port must be an integer in 0..65535, not "${rawPort}"`);
  }

  const rawHost = flagValue(argv, "host");
  if (rawHost === undefined ? argv.includes("--host") : rawHost === "") {
    return refuse("--host needs a value");
  }

  const config = loadConfig({ argv });

  // The serve path owns its own exit: it never reaches the aggregation or the write below, so the
  // offline report keeps exactly the bytes it had before there was a server.
  if (serving) {
    return await serveForeground(config.home, {
      port: rawPort === undefined ? undefined : Number(rawPort),
      host: rawHost,
      since: rawSince,
    });
  }

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

/**
 * A port is what `listen` accepts: a base-10 integer in the whole range, with `0` meaning "the kernel
 * picks one". Fractions, signs, `1e3` and the empty string are all refused rather than coerced.
 */
function isPortLiteral(text: string): boolean {
  return /^[0-9]+$/.test(text) && Number(text) <= 65535;
}

/**
 * Binds, announces, and waits to be told to stop. The announcement is the only stdout this path
 * writes, because it is the only thing a caller needs before it can open the page; everything the
 * server logs on its own goes to stderr through `#shared/log`.
 *
 * The refusal a non-loopback `--host` earns is a `CliUsageError` raised by `serveReport` before it
 * binds, so it becomes the same exit-2 answer as a bad flag instead of an uncaught throw.
 */
async function serveForeground(
  home: string,
  options: { port: number | undefined; host: string | undefined; since: string | undefined },
): Promise<number> {
  let server: ReportServer;
  try {
    // The defaults live in `serve.ts` (`DEFAULT_REPORT_PORT`, loopback), so an absent flag is an
    // absent property here rather than a second copy of the default spelled in this module.
    server = await serveReport({
      home,
      since: options.since,
      ...(options.port === undefined ? {} : { port: options.port }),
      ...(options.host === undefined ? {} : { host: options.host }),
    });
  } catch (error) {
    if (error instanceof CliUsageError) {
      process.stderr.write(`${error.message}\n${USAGE}\n`);
      return EXIT_USAGE;
    }
    throw error;
  }

  // Armed before the announcement: a stop signal that lands between the write and the `await` would
  // otherwise be the default action — a killed process reports 143 to whatever started it, and a
  // supervisor that SIGTERMs the instant it reads the line is exactly the case this verb exists for.
  const stopped = untilSignal();
  process.stdout.write(`serving ${server.url} (${server.repositories} repositories)\n`);
  await stopped;
  await server.close();
  return 0;
}

/**
 * The verb serves until it is stopped, and either of the two signals stops it: `SIGINT` is the
 * terminal, `SIGTERM` is whatever runs the verb as a service. The listeners are removed before the
 * promise settles, so a second signal is the default one (kill) rather than a second no-op.
 */
function untilSignal(): Promise<void> {
  return new Promise<void>((settle) => {
    const stop = (): void => {
      process.off("SIGINT", stop);
      process.off("SIGTERM", stop);
      settle();
    };
    process.once("SIGINT", stop);
    process.once("SIGTERM", stop);
  });
}

/** The reason first, then the block, so the last thing a caller sees is how to call it right. */
function refuse(reason: string): number {
  process.stderr.write(`${reason}\n${USAGE}\n`);
  return EXIT_USAGE;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
