import { afterAll, beforeAll, describe, expect, spyOn, test } from "bun:test";
import { mkdir, mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CliUsageError } from "#shared/cli/args";
import type { Logger } from "#shared/log";
import { writeRegistry } from "#shared/registry";
import { type CallRecord, RECORD_VERSION } from "#shared/telemetry/record";
import {
  DEFAULT_REPORT_PORT,
  LOOPBACK_HOSTS,
  type ReportServer,
  reportSections,
  serveReport,
} from "#shared/telemetry/serve";

/**
 * The report server over real HTTP on an ephemeral port. Nothing here binds a fixed port, reaches
 * the operator's home or writes a repository's `.sangkar/`: the registry, the repos and every JSONL
 * line live under `mkdtemp` directories, removed in `afterAll`, and `SANGKAR_HOME` points at the
 * temp home so a path that resolves one can never find the real installation.
 *
 * The fixtures are hand-written lines rather than records produced by the sink, because what is
 * under test is the server — the reader's own arithmetic is `telemetry-report.test.ts`'s subject.
 */

const dirs: string[] = [];
const servers: ReportServer[] = [];
let previousHome: string | undefined;

/** These cases are about HTTP and files, not about stderr. */
const silent: Logger = {
  debug: () => {},
  info: () => {},
  warn: () => {},
  error: () => {},
};

const INSTALL_ID = "0f".repeat(16);
const SESSION_ID = "1a".repeat(16);

async function tempHome(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "sangkar-serve-"));
  dirs.push(dir);
  process.env.SANGKAR_HOME = dir;
  return dir;
}

async function tempRepo(home: string, name: string): Promise<string> {
  const root = join(home, name);
  await mkdir(root, { recursive: true });
  // Canonical, because `readRegistry` canonicalises what it reads and the label carries the root.
  return realpath(root);
}

function callRecord(overrides: Partial<CallRecord> = {}): CallRecord {
  return {
    ts: "2026-11-18T10:00:00.000Z",
    v: RECORD_VERSION,
    kind: "call",
    install_id: INSTALL_ID,
    session_id: SESSION_ID,
    seq: 1,
    tool: "status",
    status: "ok",
    error_code: null,
    duration_ms: 10,
    arg_names_set: [],
    budget_tokens: 1_000,
    payload_tokens: 100,
    budget_used_ratio: 0.1,
    truncated: false,
    truncation_reason: null,
    total_relation: "gte",
    coverage_complete: true,
    coverage_indexed: 1,
    coverage_skipped: 0,
    coverage_failed: 0,
    unresolved_edges: 0,
    stale: false,
    stale_reason: null,
    generation_age_ms: null,
    result_counts: {},
    server_version: "0.1.0-beta.2",
    bun_version: "1.4.4",
    os: "linux",
    arch: "x64",
    graph_schema_version: 1,
    ...overrides,
  };
}

/** One `<date>.jsonl` under the repo's telemetry directory, appended to the existing file. */
async function writeTelemetry(repo: string, date: string, lines: readonly string[]): Promise<void> {
  const dir = join(repo, ".sangkar", "telemetry");
  await mkdir(dir, { recursive: true });
  await writeFile(join(dir, `${date}.jsonl`), `${lines.join("\n")}\n`);
}

async function appendTelemetry(repo: string, date: string, line: string): Promise<void> {
  await writeFile(join(repo, ".sangkar", "telemetry", `${date}.jsonl`), `${line}\n`, { flag: "a" });
}

interface Fixture {
  readonly home: string;
  readonly alpha: string;
  readonly beta: string;
  readonly empty: string;
  readonly registry: readonly string[];
}

/**
 * Three repos: `alpha` has two records and one torn line, `beta` has one record, `empty` has no
 * telemetry directory at all. The torn line is deliberate — the machine total's `unparsed` has to
 * be the sum over repositories, not the count of a single directory.
 */
async function fixture(): Promise<Fixture> {
  const home = await tempHome();
  const alpha = await tempRepo(home, "alpha");
  const beta = await tempRepo(home, "beta");
  const empty = await tempRepo(home, "empty");

  await writeTelemetry(alpha, "2026-11-18", [
    JSON.stringify(callRecord({ tool: "status" })),
    JSON.stringify(callRecord({ tool: "search", seq: 2 })),
    "{not json",
  ]);
  await writeTelemetry(beta, "2026-11-18", [JSON.stringify(callRecord({ tool: "trace" }))]);

  await writeRegistry(join(home, "sangkar.json"), { version: 1, repos: [alpha, beta, empty] });
  return { home, alpha, beta, empty, registry: [alpha, beta, empty] };
}

async function started(options: Parameters<typeof serveReport>[0]): Promise<ReportServer> {
  const server = await serveReport(options);
  servers.push(server);
  return server;
}

beforeAll(() => {
  previousHome = process.env.SANGKAR_HOME;
});

afterAll(async () => {
  // Every listener first: a request still in flight would otherwise race the directory removal.
  await Promise.all(servers.map((server) => server.close()));
  servers.length = 0;
  if (previousHome === undefined) {
    delete process.env.SANGKAR_HOME;
  } else {
    process.env.SANGKAR_HOME = previousHome;
  }
  await Promise.all(dirs.map((dir) => rm(dir, { recursive: true, force: true })));
  dirs.length = 0;
});

describe("reportSections", () => {
  test("one section per registered root, labelled with its basename and path", async () => {
    const { home, alpha, beta, empty } = await fixture();
    const { sections } = reportSections(home, null);

    expect(sections.map((section) => section.label)).toEqual([
      `alpha — ${alpha}`,
      `beta — ${beta}`,
      `empty — ${empty}`,
    ]);
    expect(sections.map((section) => section.sourceDir)).toEqual(
      [alpha, beta, empty].map((root) => join(root, ".sangkar", "telemetry")),
    );
  });

  test("per-repo record counts, and a machine total that sums them with every unparsed line", async () => {
    const { home } = await fixture();
    const { machine, sections } = reportSections(home, null);

    expect(sections.map((section) => section.data.totals.records)).toEqual([2, 1, 0]);
    expect(sections.map((section) => section.data.totals.unparsed)).toEqual([1, 0, 0]);
    expect(machine.totals.records).toBe(3);
    expect(machine.totals.unparsed).toBe(1);
    // The machine fold is a real aggregate, not a sum of section totals: tool names come through.
    expect(machine.tools.map((tool) => tool.tool)).toEqual(["search", "status", "trace"]);
  });

  test("a repo whose telemetry directory is missing is a section with zero records", async () => {
    const { home, empty } = await fixture();
    const { sections } = reportSections(home, null);
    const section = sections.find((entry) => entry.label.startsWith("empty — "));

    expect(section?.sourceDir).toBe(join(empty, ".sangkar", "telemetry"));
    expect(section?.data.totals.records).toBe(0);
    expect(section?.data.window.first_ts).toBeNull();
  });

  test("an empty registry is an empty report, not an error", async () => {
    const home = await tempHome();
    await writeRegistry(join(home, "sangkar.json"), { version: 1, repos: [] });

    const { machine, sections } = reportSections(home, null);
    expect(sections).toEqual([]);
    expect(machine.totals.records).toBe(0);
  });

  test("`since` narrows every section and the machine total alike", async () => {
    const home = await tempHome();
    const repo = await tempRepo(home, "dated");
    await writeTelemetry(repo, "2026-11-01", [
      JSON.stringify(callRecord({ ts: "2026-11-01T09:00:00.000Z" })),
    ]);
    await appendTelemetry(
      repo,
      "2026-11-01",
      JSON.stringify(callRecord({ ts: "2026-11-19T09:00:00.000Z", seq: 2 })),
    );
    await writeRegistry(join(home, "sangkar.json"), { version: 1, repos: [repo] });

    const all = reportSections(home, null);
    expect(all.machine.totals.records).toBe(2);

    const narrowed = reportSections(home, "2026-11-15");
    expect(narrowed.machine.totals.records).toBe(1);
    expect(narrowed.sections[0]?.data.totals.records).toBe(1);
    expect(narrowed.machine.window.since).toBe("2026-11-15");

    // `Nd` is read against the injected clock, which is the only reason `now` exists on the signature.
    const recent = reportSections(home, "7d", () => new Date("2026-11-20T12:00:00.000Z"));
    expect(recent.machine.totals.records).toBe(1);
  });
});

describe("serveReport", () => {
  test("GET / renders every repository plus the machine total", async () => {
    const { home, alpha, beta } = await fixture();
    const server = await started({ home, port: 0, log: silent });

    expect(server.port).toBeGreaterThan(0);
    expect(server.host).toBe("127.0.0.1");
    expect(server.repositories).toBe(3);
    expect(server.url).toBe(`http://127.0.0.1:${server.port}/`);

    const response = await fetch(server.url);
    const body = await response.text();
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toContain("text/html");
    expect(body).toContain(`alpha — ${alpha}`);
    expect(body).toContain(`beta — ${beta}`);
    // The machine panel names the total, which no single section can: 2 + 1 + 0.
    expect(body).toContain("records <span>3</span>");
    // Self-contained: the page must draw itself with no network beyond this listener.
    expect(body).not.toContain("http://");
    expect(body).not.toContain("<link");
  });

  test("GET /healthz answers ok as plain text", async () => {
    const { home } = await fixture();
    const server = await started({ home, port: 0, log: silent });

    const response = await fetch(`${server.url}healthz`);
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toContain("text/plain");
    expect(await response.text()).toBe("ok\n");
  });

  test("an unknown path is a 404", async () => {
    const { home } = await fixture();
    const server = await started({ home, port: 0, log: silent });

    const response = await fetch(`${server.url}nope`);
    expect(response.status).toBe(404);
    expect(await response.text()).toBe("not found\n");
  });

  test("HEAD / is a 200 with an empty body", async () => {
    const { home } = await fixture();
    const server = await started({ home, port: 0, log: silent });

    const response = await fetch(server.url, { method: "HEAD" });
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toContain("text/html");
    expect(await response.text()).toBe("");
  });

  test("records written after the bind show up on the next request", async () => {
    const { home, alpha } = await fixture();
    const server = await started({ home, port: 0, log: silent });

    expect(await (await fetch(server.url)).text()).toContain("records <span>3</span>");

    await appendTelemetry(alpha, "2026-11-18", JSON.stringify(callRecord({ seq: 3 })));

    const body = await (await fetch(server.url)).text();
    expect(body).toContain("records <span>4</span>");
    // The repository count is the bind-time answer and does not move mid-request.
    expect(server.repositories).toBe(3);
  });

  test("a non-loopback host is refused before anything binds", async () => {
    const { home } = await fixture();
    const serve = spyOn(Bun, "serve");
    let refused: unknown;
    try {
      await serveReport({ home, port: 0, host: "0.0.0.0", log: silent });
    } catch (error) {
      refused = error;
    } finally {
      // Restored before the assertions so a failing case cannot leave Bun.spy'd for the next one.
      serve.mockRestore();
    }

    expect(refused).toBeInstanceOf(CliUsageError);
    // The refusal names every allowed host, so the message is the fix and not just a "no".
    const message = refused instanceof Error ? refused.message : "";
    for (const host of LOOPBACK_HOSTS) {
      expect(message).toContain(host);
    }
    expect(message).toContain("0.0.0.0");
    expect(message).toContain("loopback only");
    expect((refused as CliUsageError).usage).toContain("--host");
    // No socket was opened and nothing was left to close.
    expect(serve).not.toHaveBeenCalled();
  });

  test("the default port is 7317 and the default host is loopback", () => {
    // The constant, not a bound listener: this file never binds the default port.
    expect(DEFAULT_REPORT_PORT).toBe(7317);
    expect(LOOPBACK_HOSTS).toEqual(["127.0.0.1", "::1", "localhost"]);
  });

  test("close() is idempotent and the listener stops answering", async () => {
    const { home } = await fixture();
    const server = await started({ home, port: 0, log: silent });
    const url = server.url;

    await server.close();
    await server.close();
    // The promise is shared, so both calls waited on the same shutdown.
    expect(server.close()).toBe(server.close());

    let answered = true;
    try {
      await fetch(`${url}healthz`);
    } catch {
      answered = false;
    }
    expect(answered).toBe(false);
  });
});
