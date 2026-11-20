/**
 * The multi-repo report, served: one loopback listener that renders every registered repository's
 * telemetry directory on every request.
 *
 * Nothing here folds or renders. `report.ts` reads and aggregates, `html-multi.ts` draws the page,
 * and `registry.ts` owns the list of trees — this module is the seam that joins the three, plus the
 * one policy a server has to hold on its own: it binds loopback and nothing else, because a report
 * of every repository on the machine is not something the network gets to read.
 *
 * The document is recomputed per request rather than at bind time: a daemon serves for days, and a
 * page rendered once would stop mentioning the records written after it started.
 */
import { basename, join } from "node:path";
import { CliUsageError } from "#shared/cli/args";
import { type Logger, log } from "#shared/log";
import { readRegistry } from "#shared/registry";
import { type ReportSectionView, renderMultiReport } from "./html-multi";
import type { TelemetryRecord } from "./record";
import { aggregate, parseSince, type ReportData, readRecords } from "./report";

/**
 * A default, never a promise: a caller that passes `0` gets an ephemeral port instead, which is what
 * every test does so no case can collide with a real daemon or with the other cases.
 */
export const DEFAULT_REPORT_PORT = 7317;

/**
 * The whole allow-list. `localhost` is here because it is the spelling a person types; a hostname
 * that resolves to loopback is not, because the check would then have to be a DNS lookup whose
 * answer can change between the check and the bind.
 */
export const LOOPBACK_HOSTS = ["127.0.0.1", "::1", "localhost"] as const;

const DEFAULT_HOST = "127.0.0.1";
const REGISTRY_FILE = "sangkar.json";
const DATA_DIR = ".sangkar";
const TELEMETRY_DIR = "telemetry";

/**
 * Printed after a refusal, so the caller learns the allowed spellings from the same answer. The verb
 * is `sangkar telemetry --serve`: `serveReport` has no verb of its own, and a usage line naming a
 * command that does not exist is worse than no line at all.
 */
const USAGE = `usage: sangkar telemetry --serve [--host <${LOOPBACK_HOSTS.join(" | ")}>] [--port <port>] [--since <YYYY-MM-DD | Nd>]
  --host   the loopback address to bind (default: ${DEFAULT_HOST})
  --port   the port to bind (default: ${DEFAULT_REPORT_PORT}; 0 asks the kernel for one)
  --since  include records from that day (YYYY-MM-DD) or the last N days (Nd); default: all`;

const HTML_HEADERS = { "content-type": "text/html; charset=utf-8" } as const;
const TEXT_HEADERS = { "content-type": "text/plain; charset=utf-8" } as const;

export interface ServeOptions {
  /** The installation's home; `<home>/sangkar.json` is the registry. Never read from the environment. */
  readonly home: string;
  readonly port?: number;
  readonly host?: string;
  /** `YYYY-MM-DD` or `Nd`, spelled as `sangkar telemetry --since` spells it. */
  readonly since?: string | undefined;
  readonly now?: () => Date;
  readonly log?: Logger;
}

export interface ReportServer {
  /** The URL the listener answers on (`http://<host>:<port>/`), which is how `port: 0` is read back. */
  readonly url: string;
  readonly port: number;
  readonly host: string;
  /** The registry's repository count at bind time; the routes read the registry again per request. */
  readonly repositories: number;
  close(): Promise<void>;
}

export interface ReportSections {
  /** The machine total: every section's records in one fold, with the summed unparsed count. */
  readonly machine: ReportData;
  readonly sections: readonly ReportSectionView[];
}

/**
 * One section per registered root, in registry order. A root with no telemetry directory is still
 * a section: "this repository recorded nothing" is an answer, and dropping it would hide a repo
 * that was registered but never used. The label is `<basename> — <root>` because a basename alone
 * collides the moment two trees are called `app`, and the renderer escapes it.
 */
export function reportSections(
  home: string,
  since: string | null,
  now: () => Date = () => new Date(),
): ReportSections {
  const registry = readRegistry(join(home, REGISTRY_FILE));
  // One clock reading for the whole report: `Nd` is one window, not one window per repository.
  const window = parseSince(since ?? undefined, now());

  const sections: ReportSectionView[] = [];
  const records: TelemetryRecord[] = [];
  const files: string[] = [];
  let unparsed = 0;

  for (const root of registry.repos) {
    const sourceDir = join(root, DATA_DIR, TELEMETRY_DIR);
    const read = readRecords(sourceDir, { since: window });
    unparsed += read.unparsed;
    files.push(...read.files);
    for (const record of read.records) {
      records.push(record);
    }
    sections.push({
      label: `${basename(root)} — ${root}`,
      sourceDir,
      data: aggregate({
        records: read.records,
        unparsed: read.unparsed,
        files: read.files,
        since,
      }),
    });
  }

  return { machine: aggregate({ records, unparsed, files, since }), sections };
}

/** Membership in the literal allow-list, so the refusal is decided from the string alone. */
function isLoopbackHost(host: string): boolean {
  return (LOOPBACK_HOSTS as readonly string[]).includes(host);
}

/**
 * Binds the listener and answers with its origin. The host is validated before `Bun.serve` is
 * reached, so a refusal leaves no socket behind and no half-started server to close.
 */
export async function serveReport(options: ServeOptions): Promise<ReportServer> {
  const host = options.host ?? DEFAULT_HOST;
  if (!isLoopbackHost(host)) {
    throw new CliUsageError(
      `the report server binds loopback only: --host must be one of ${LOOPBACK_HOSTS.join(", ")}, not "${host}"`,
      USAGE,
    );
  }

  const port = options.port ?? DEFAULT_REPORT_PORT;
  const logger = options.log ?? log;
  const now = options.now ?? (() => new Date());
  const since = options.since ?? null;
  const repositories = readRegistry(join(options.home, REGISTRY_FILE), { log: logger }).repos
    .length;

  const server = Bun.serve({
    hostname: host,
    port,
    fetch: (request) => respond(request, options.home, since, now),
  });

  logger.info("report server listening", {
    url: server.url.href,
    host,
    port: server.port ?? port,
    repositories,
  });

  // Bound once: `stop(true)` is already idempotent, but one shared promise means two callers close
  // the same listener instead of racing two shutdowns.
  let stopping: Promise<void> | null = null;
  return {
    url: server.url.href,
    port: server.port ?? port,
    host,
    repositories,
    close: () => {
      stopping ??= server.stop(true);
      return stopping;
    },
  };
}

/**
 * Three routes, matched literally. `HEAD /healthz` is a 404 on purpose: the contract lists the
 * method of every route, and a probe that wants to know whether the page renders has to read it.
 */
function respond(request: Request, home: string, since: string | null, now: () => Date): Response {
  const { pathname } = new URL(request.url);

  if (request.method === "GET" && pathname === "/healthz") {
    return new Response("ok\n", { headers: TEXT_HEADERS });
  }

  if (pathname === "/") {
    if (request.method === "HEAD") {
      // Status and headers are the whole answer to a HEAD; rendering the page would be work no
      // client reads, so the document is not built here.
      return new Response(null, { headers: HTML_HEADERS });
    }
    if (request.method === "GET") {
      const { machine, sections } = reportSections(home, since, now);
      const html = renderMultiReport({
        machine,
        sections,
        generated_at: now().toISOString(),
        since,
      });
      return new Response(html, { headers: HTML_HEADERS });
    }
  }

  return new Response("not found\n", { status: 404, headers: TEXT_HEADERS });
}
