/**
 * Whether an `index` run may start: the one place that decides it, so the answer is a pure function
 * of five facts and the ask-the-operator step is a single, injectable call.
 *
 * The tree is guessed by a walk (`config.ts`'s `detectTree`), and a guess is what a `.sangkar` in
 * `$HOME` or a directory at the wrong end of a symlink punishes: the run reads a tree nobody chose,
 * and — before this gate — reported it only after the fact. The rule below never refuses a tree the
 * caller named, and never refuses the directory it is standing in; what it refuses is a tree found
 * *above* the working directory that nothing vouched for, when there is no one to ask.
 */
import { createInterface } from "node:readline";
import { MAX_AUTODETECT_LEVELS } from "#shared/config";

/** What the process should do with an autodetected tree. */
export type IndexGate =
  | { readonly action: "run" }
  | { readonly action: "ask" }
  | { readonly action: "refuse"; readonly reason: "unregistered" | "too_deep" };

export interface IndexGateInput {
  /** `--repo-root` named the tree, so nothing was guessed. */
  readonly explicit: boolean;
  /** `--yes`: the operator states that the guessed tree is the one they mean. */
  readonly yes: boolean;
  /** Directories between the working directory and the tree; `0` is the working directory itself. */
  readonly levels: number;
  /** Whether the process can ask at all: a TTY on stdin. */
  readonly tty: boolean;
  /** Whether the machine registry already names this tree. */
  readonly registered: boolean;
}

/**
 * The decision, in the order the reasons win:
 *
 * 1. a named tree, or `--yes` — the operator spoke, so nothing is asked and nothing refused;
 * 2. `levels === 0` — the working directory is the tree, which is a first index, not a guess;
 * 3. a TTY — ask, because a guess above the working directory deserves a question;
 * 4. no TTY, tree registered and within {@link MAX_AUTODETECT_LEVELS} — proceed, because the registry
 *    is a prior statement of intent and a script must keep working;
 * 5. otherwise refuse: unregistered, or deeper than the walk is allowed to reach unattended.
 */
export function indexGate(input: IndexGateInput): IndexGate {
  if (input.explicit || input.yes) {
    return { action: "run" };
  }
  if (input.levels === 0) {
    return { action: "run" };
  }
  if (input.tty) {
    return { action: "ask" };
  }
  if (input.registered && input.levels <= MAX_AUTODETECT_LEVELS) {
    return { action: "run" };
  }
  return { action: "refuse", reason: input.registered ? "too_deep" : "unregistered" };
}

export interface RefusalInput {
  readonly root: string;
  readonly cwd: string;
  readonly levels: number;
  /** `declined` is the operator answering no, which no hint should talk them out of. */
  readonly reason: "unregistered" | "too_deep" | "declined";
}

/**
 * The three lines a refusal writes to stderr: what was refused, and the ways to say it anyway. It names
 * the directory the caller is in as well as the one that was refused, because the fix is almost always
 * `--repo-root .` — the operator meant the tree they were standing in.
 */
export function refusalMessage(input: RefusalInput): string {
  const why =
    input.reason === "too_deep"
      ? `${input.root} is ${input.levels} levels above ${input.cwd} (max ${MAX_AUTODETECT_LEVELS})`
      : input.reason === "declined"
        ? `${input.root} was not confirmed`
        : `${input.root} is not registered as a repository`;
  return [
    `refused: ${why}. Nothing was indexed.`,
    "  run `sangkar index --repo-root .` to index the directory you are in,",
    "  or `sangkar repo add <path>` to register the tree you meant,",
    `  or pass \`--yes\` if ${input.root} is really that tree.`,
  ].join("\n");
}

/** The question the gate asks when it can: one line, and `N` is what Enter means. */
export const CONTINUE_QUESTION = "continue with this repo? (y/N) ";

export interface AskOptions {
  /** Injected by tests; the process's stdin in production. */
  readonly input?: NodeJS.ReadableStream;
  /** Injected by tests; stderr in production, because stdout belongs to the protocol and the answer. */
  readonly output?: NodeJS.WritableStream;
}

/**
 * One yes/no line. Only `y` and `yes` say yes; Enter, anything else, and a stdin that closes without
 * an answer all say no, so a prompt that cannot be answered can never approve a run.
 */
export async function askContinue(options: AskOptions = {}): Promise<boolean> {
  const input = options.input ?? process.stdin;
  const output = options.output ?? process.stderr;
  output.write(CONTINUE_QUESTION);

  const reader = createInterface({ input, terminal: false });
  try {
    const answer = await new Promise<string | null>((resolve) => {
      let settled = false;
      const settle = (value: string | null): void => {
        if (!settled) {
          settled = true;
          resolve(value);
        }
      };
      reader.once("line", (line) => settle(line));
      reader.once("close", () => settle(null));
    });
    return answer !== null && /^y(es)?$/i.test(answer.trim());
  } finally {
    reader.close();
  }
}
