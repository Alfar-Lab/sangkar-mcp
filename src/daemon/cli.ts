/**
 * `sangkar daemon`: the four subcommands an operator needs to run the background process — or to
 * find out why it is not doing what they expect.
 *
 * The module is deliberately thin. `./lifecycle` owns the pid file and the three lifecycle verbs,
 * `./main` owns the process's composition, and `#shared/config.ts` owns the home; this file only
 * names the subcommand, resolves the two flags `run` takes, and turns the answer into one line of
 * stdout plus an exit code (0 ok, 1 error or not running, 2 usage refusal).
 *
 * `run` is the process `start` spawns, so it must never print anything on stdout that `start`'s
 * detached child would then write into the log: everything the running daemon says goes through
 * `#shared/log` on stderr, and the only stdout here belongs to the command that returns.
 */
import { CliUsageError } from "#shared/cli/args";
import { EXIT_FAILURE, EXIT_USAGE } from "#shared/cli/render";
import { flagValue, loadConfig } from "#shared/config";
import { isAppError } from "#shared/errors";
import { log } from "#shared/log";
import { readPidLock } from "#shared/pidlock";
import { daemonStatus, startDaemon, stopDaemon } from "./lifecycle";
import { runDaemonForeground } from "./main";

/**
 * Printed after every refusal. The same text is `VERB_USAGE.daemon` in `bin/sangkar.ts`, which
 * answers `--help` before dispatch; a test pins the two together.
 */
const USAGE = `usage: sangkar daemon [run|start|stop|status] [--port <n>] [--host <addr>]
  run     run the daemon in the foreground until a signal stops it
  start   start it as a detached child and return once it holds the pid file
  stop    stop the daemon this home's pid file names
  status  one header line, then one line per registered repository
  --port <n>     the report server's port (default: 7317; 0 picks a free port)
  --host <addr>  the address to bind (default: 127.0.0.1; loopback only)
  --port and --host belong to "run"; start runs it with the defaults`;

export async function runDaemonCli(argv: readonly string[]): Promise<number> {
  // Answered before any subcommand is resolved, so `daemon --help` never reaches a lock or a bind.
  if (argv.includes("--help") || argv.includes("-h")) {
    process.stdout.write(`${USAGE}\n`);
    return 0;
  }

  const [verb, ...rest] = argv;

  switch (verb) {
    case "run":
      return await run(rest, argv);

    case "start":
      return await start(rest, argv);

    case "stop":
      return await stop(rest, argv);

    case "status":
      return await status(rest, argv);

    case undefined:
      return refuse("daemon needs a subcommand");

    default:
      return refuse(`unknown subcommand "${verb}"`);
  }
}

/**
 * The foreground process. `home` comes from `#shared/config.ts` and nowhere else, so the pid file
 * this acquires is the one `start`, `stop` and `status` read.
 *
 * Two refusals are caught here because the process is long-lived and a stack trace would be the
 * whole answer: a bad `--host`/`--port` is a `CliUsageError` from the report server (exit 2), and a
 * pid file another daemon holds is an `index_locked` `AppError` (exit 1). Both leave the home as
 * they found it — the server validates before it binds, and the lock precedes everything.
 */
async function run(rest: readonly string[], argv: readonly string[]): Promise<number> {
  const stray = unknownFlag(rest, ["port", "host"]);
  if (stray !== null) {
    return refuse(`"daemon run" takes no ${stray}`);
  }

  const rawPort = flagValue(argv, "port");
  if (rawPort === undefined) {
    if (argv.includes("--port")) {
      return refuse("--port needs a value");
    }
  } else if (!/^[0-9]+$/.test(rawPort) || Number(rawPort) > 65535) {
    return refuse(`--port must be an integer in 0..65535, not "${rawPort}"`);
  }
  const rawHost = flagValue(argv, "host");
  if (rawHost === undefined ? argv.includes("--host") : rawHost === "") {
    return refuse("--host needs a value");
  }

  const config = loadConfig({ argv });
  try {
    return await runDaemonForeground({
      home: config.home,
      // `exactOptionalPropertyTypes`: an absent flag is an absent property, not `undefined`.
      ...(rawPort === undefined ? {} : { port: Number(rawPort) }),
      ...(rawHost === undefined ? {} : { host: rawHost }),
    });
  } catch (error) {
    if (error instanceof CliUsageError) {
      return refuse(error.message);
    }
    if (isAppError(error)) {
      return fail(error.message, error.hint);
    }
    throw error;
  }
}

/** A detached start: the child's pid is read back from the pid file it wrote, not from the spawn. */
async function start(rest: readonly string[], argv: readonly string[]): Promise<number> {
  const stray = unknownFlag(rest, []);
  if (stray !== null) {
    return refuse(`"daemon start" takes no ${stray}`);
  }

  const config = loadConfig({ argv });
  if ((await startDaemon(config.home)) !== 0) {
    // The lifecycle logged the reason; a second line on stdout would be a claim it did not make.
    return EXIT_FAILURE;
  }

  const state = await readPidLock(config.daemon.pidPath);
  if (state.pid === null) {
    return fail("the daemon reported a start but wrote no pid file");
  }
  process.stdout.write(`started (pid ${state.pid})\n`);
  return 0;
}

/**
 * A stop is only announced once the pid file is gone — `stopDaemon` waits for exactly that — and the
 * pid printed is the one read before the signal, because the file that held it no longer exists.
 */
async function stop(rest: readonly string[], argv: readonly string[]): Promise<number> {
  const stray = unknownFlag(rest, []);
  if (stray !== null) {
    return refuse(`"daemon stop" takes no ${stray}`);
  }

  const config = loadConfig({ argv });
  const state = await readPidLock(config.daemon.pidPath);
  if (!state.held || state.pid === null) {
    process.stdout.write("not running\n");
    return EXIT_FAILURE;
  }

  const pid = state.pid;
  if ((await stopDaemon(config.home)) !== 0) {
    return EXIT_FAILURE;
  }
  process.stdout.write(`stopped (pid ${pid})\n`);
  return 0;
}

/** The answer an operator reads first, and the exit code `stop` would give: 0 only while running. */
async function status(rest: readonly string[], argv: readonly string[]): Promise<number> {
  const stray = unknownFlag(rest, []);
  if (stray !== null) {
    return refuse(`"daemon status" takes no ${stray}`);
  }

  const config = loadConfig({ argv });
  const answer = await daemonStatus(config.home);
  const running = answer.pid !== null;
  const lines = [running ? `running, pid ${answer.pid}` : "not running"];
  for (const repo of answer.repos) {
    lines.push(
      `  ${repo.root}  watch=${onOff(repo.watch)} interval=${repo.watchIntervalMs}ms telemetry=${onOff(repo.telemetry)}`,
    );
  }
  process.stdout.write(`${lines.join("\n")}\n`);
  return running ? 0 : EXIT_FAILURE;
}

/**
 * The first flag a subcommand does not read, or `null`. `run` reads `--port` and `--host`; the other
 * three read none, and a silently ignored `--port` on `start` would look like it had been applied.
 * A flag's value is a bare token and is skipped.
 */
function unknownFlag(argv: readonly string[], allowed: readonly string[]): string | null {
  for (const token of argv) {
    if (!token.startsWith("--")) {
      continue;
    }
    const name = token.slice(2).split("=")[0] ?? "";
    if (!allowed.includes(name)) {
      return token;
    }
  }
  return null;
}

function onOff(value: boolean): string {
  return value ? "on" : "off";
}

/** The reason first, then the block, so the last thing a caller sees is how to call it right. */
function refuse(reason: string): number {
  process.stderr.write(`${reason}\n${USAGE}\n`);
  return EXIT_USAGE;
}

/** A failure with no usage to offer: the verb was called correctly and the daemon or fs refused. */
function fail(reason: string, hint?: string): number {
  log.error("daemon cli refused", { message: reason });
  process.stderr.write(`sangkar daemon: ${reason}\n`);
  if (hint !== undefined) {
    process.stderr.write(`${hint}\n`);
  }
  return EXIT_FAILURE;
}
