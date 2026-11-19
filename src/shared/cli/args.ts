/**
 * The command line's argument layer: one parser per tool, driven by the same manifest literal the
 * MCP surface advertises.
 *
 * `parseArgs`, `matchesType`, `matchesItem` and `expectationOf` are moved verbatim from
 * `src/index.ts`, where they validate a protocol call. Keeping one copy is the point: a `sangkar
 * <tool>` invocation and a tool call must accept and refuse exactly the same arguments, and a
 * second, laxer schema on the CLI side is the drift this module exists to prevent. What is new
 * here is only the argv reading — `splitGlobals` takes the flags no tool declares, and
 * `parseCliArgs` turns the remaining tokens into the record `parseArgs` already knows how to
 * check.
 */
import { AppError } from "#shared/errors";

/**
 * One declared argument, exactly the literal a manifest exports (`TECH-SPEC.md` §6). Deliberately
 * loose — `type` is a plain string — because one manifest writes its table without `as const`, so a
 * stricter union would need a cast here to say what the switch below already checks at runtime.
 */
export interface InputField {
  readonly type: string;
  readonly required?: boolean;
  readonly default?: unknown;
  readonly min?: number;
  readonly max?: number;
  readonly values?: readonly string[];
  readonly items?: { readonly type?: string; readonly values?: readonly string[] };
  readonly description?: string;
}

export type InputLiteral = Readonly<Record<string, InputField>>;

/** The argument map a validated call hands a handler: declared keys only, defaults applied. */
export type ToolArgs = Readonly<Record<string, unknown>>;

/**
 * The flags that sit before the tool token and belong to no tool's schema, so no manifest can
 * declare or refuse them. `--json` and `--help` are switches; these two carry a value.
 */
export const GLOBAL_VALUE_FLAGS = ["repo-root", "data-dir"] as const;

/** What a refusal from {@link splitGlobals} points at, since no tool is known yet. */
const GLOBAL_USAGE =
  "sangkar [--repo-root <path>] [--data-dir <path>] [--json] [--help] <tool> [arguments]";

/**
 * A command line the schema cannot accept, carrying the usage block the caller prints. Separate
 * from `AppError` because this failure is the argv, not the call: it never crosses the MCP
 * boundary, and its refusal is answered with a usage block rather than a retry hint.
 */
export class CliUsageError extends Error {
  readonly usage: string;

  constructor(message: string, usage: string) {
    super(message);
    this.name = "CliUsageError";
    this.usage = usage;
  }
}

function isGlobalValueFlag(name: string): boolean {
  return (GLOBAL_VALUE_FLAGS as readonly string[]).includes(name);
}

/** The token's name part, without `--` and without a `=value` tail. */
function nameOf(token: string): string {
  const eq = token.indexOf("=");
  return eq === -1 ? token.slice(2) : token.slice(2, eq);
}

/** The token's `=value` tail, or `undefined` when it was written as a separate token. */
function inlineValueOf(token: string): string | undefined {
  const eq = token.indexOf("=");
  return eq === -1 ? undefined : token.slice(eq + 1);
}

/**
 * Removes the global flags from argv and reports the two switches. The scan is order-preserving
 * and leaves everything else untouched, so the tokens a tool still has to read arrive in the
 * order the user wrote them. A value flag with nothing to consume is refused here rather than
 * silently treated as a switch, which is the failure a user only notices after the index was
 * built in the wrong place. A token beginning with `--` is never taken as a value; a single dash
 * is, so a negative number stays a value.
 */
export function splitGlobals(argv: readonly string[]): {
  readonly json: boolean;
  readonly help: boolean;
  readonly rest: readonly string[];
} {
  let json = false;
  let help = false;
  const rest: string[] = [];

  for (let i = 0; i < argv.length; i += 1) {
    const token = argv[i] ?? "";
    if (token === "--json") {
      json = true;
      continue;
    }
    if (token === "--help" || token === "-h") {
      help = true;
      continue;
    }
    if (!token.startsWith("--") || !isGlobalValueFlag(nameOf(token))) {
      rest.push(token);
      continue;
    }
    const inline = inlineValueOf(token);
    if (inline !== undefined) {
      // `--repo-root=` names the flag and withholds the value, which is a typo, not an empty root.
      if (inline === "") {
        throw new CliUsageError(`--${nameOf(token)} needs a value`, GLOBAL_USAGE);
      }
      continue;
    }
    const next = argv[i + 1];
    if (next === undefined || next.startsWith("--")) {
      throw new CliUsageError(`--${nameOf(token)} needs a value`, GLOBAL_USAGE);
    }
    i += 1;
  }

  return { json, help, rest };
}

/**
 * The one field a bare token may bind: the single required string argument of the tool. `trace`,
 * `search` and `expand` each declare exactly one, which is what makes `sangkar search pom.xml` the
 * obvious spelling; a literal whose required field is an enum, or which has two required fields,
 * gets no positional so nothing is guessed.
 */
function positionalFieldOf(input: InputLiteral): string | undefined {
  const required = Object.entries(input).filter(
    ([, field]) => field.required === true && field.type === "string",
  );
  return required.length === 1 ? required[0]?.[0] : undefined;
}

function matchesItem(field: InputField, value: unknown): boolean {
  if (typeof value !== "string") {
    return false;
  }
  const values = field.items?.values;
  return values === undefined || values.includes(value);
}

function matchesType(field: InputField, value: unknown): boolean {
  switch (field.type) {
    case "string":
      return typeof value === "string";
    case "integer":
      return typeof value === "number" && Number.isInteger(value);
    case "boolean":
      return typeof value === "boolean";
    case "enum":
      return typeof value === "string" && (field.values ?? []).includes(value);
    case "array":
      return Array.isArray(value) && value.every((item) => matchesItem(field, item));
    default:
      // A manifest field with an unknown `type` is a bug in the tool, never in the call.
      return false;
  }
}

/** What the argument should have been, in the one line the agent's retry needs. */
function expectationOf(name: string, field: InputField): string {
  switch (field.type) {
    case "integer": {
      const floor = field.min ?? Number.MIN_SAFE_INTEGER;
      const ceiling = field.max ?? Number.MAX_SAFE_INTEGER;
      return `"${name}" must be an integer between ${floor} and ${ceiling}`;
    }
    case "enum":
      return `"${name}" must be one of: ${(field.values ?? []).join(", ")}`;
    case "array": {
      const values = field.items?.values;
      return values === undefined
        ? `"${name}" must be an array of strings`
        : `"${name}" must be an array of: ${values.join(", ")}`;
    }
    case "boolean":
      return `"${name}" must be true or false`;
    default:
      return `"${name}" must be a string`;
  }
}

/**
 * Arguments against the manifest literal: unknown keys are dropped, declared defaults fill an
 * absent argument, a missing required argument and a wrong type are refused. Refusals carry the
 * closed `internal` code because they are our own schema disagreeing with the client, and the hint
 * names the argument so the retry is a correction rather than a guess.
 */
export function parseArgs(tool: string, input: InputLiteral, raw: unknown): ToolArgs {
  const source: Record<string, unknown> =
    typeof raw === "object" && raw !== null && !Array.isArray(raw)
      ? (raw as Record<string, unknown>)
      : {};
  const args: Record<string, unknown> = {};
  for (const [name, field] of Object.entries(input)) {
    const value = source[name];
    if (value === undefined) {
      if (field.default !== undefined) {
        args[name] = field.default;
      } else if (field.required === true) {
        throw new AppError("internal", `${tool} is missing the required argument "${name}"`, {
          hint: expectationOf(name, field),
        });
      }
      continue;
    }
    if (!matchesType(field, value)) {
      throw new AppError("internal", `${tool} received an invalid "${name}" argument`, {
        hint: expectationOf(name, field),
      });
    }
    args[name] = value;
  }
  return args;
}

/**
 * argv read into the record {@link parseArgs} validates: the declared names the caller wrote and
 * nothing else, since an unknown option is refused rather than kept. It is therefore also the answer
 * to "which arguments were supplied" — a default is added afterwards, by `parseArgs`, and a defaulted
 * name is not a fact about the request.
 */
function readArgv(
  tool: string,
  input: InputLiteral,
  argv: readonly string[],
): Record<string, unknown> {
  const usage = usageOf(tool, input);
  const positional = positionalFieldOf(input);
  const raw: Record<string, unknown> = {};

  for (let i = 0; i < argv.length; i += 1) {
    const token = argv[i] ?? "";
    if (!token.startsWith("--")) {
      if (positional === undefined || raw[positional] !== undefined) {
        throw new CliUsageError(`unexpected positional argument "${token}"`, usage);
      }
      raw[positional] = token;
      continue;
    }

    const written = nameOf(token);
    const inline = inlineValueOf(token);
    // `--no-force` is the switch spelling of `--force=false`; a field literally named `no-…` wins.
    const negated =
      inline === undefined && written.startsWith("no-") ? written.slice(3) : undefined;
    const declared = input[written];
    const name = declared === undefined && negated !== undefined ? negated : written;
    const field = declared ?? input[name];
    if (field === undefined) {
      throw new CliUsageError(`unknown option "--${written}"`, usage);
    }

    if (name !== written) {
      if (field.type !== "boolean") {
        throw new CliUsageError(`unknown option "--${written}"`, usage);
      }
      raw[name] = false;
      continue;
    }

    if (field.type === "boolean" && inline === undefined) {
      raw[name] = true;
      continue;
    }
    if (field.type === "boolean") {
      if (inline !== "true" && inline !== "false") {
        throw new CliUsageError(`--${name} must be true or false`, usage);
      }
      raw[name] = inline === "true";
      continue;
    }

    let value = inline;
    if (value === "") {
      // `--depth=` withholds the value rather than passing the empty string: no argument here is one.
      throw new CliUsageError(`--${name} needs a value`, usage);
    }
    if (value === undefined) {
      const next = argv[i + 1];
      if (next === undefined || next.startsWith("--")) {
        throw new CliUsageError(`--${name} needs a value`, usage);
      }
      value = next;
      i += 1;
    }

    if (field.type === "integer") {
      const parsed = Number(value);
      if (!Number.isInteger(parsed)) {
        throw new CliUsageError(`--${name} must be an integer`, usage);
      }
      raw[name] = parsed;
      continue;
    }
    if (field.type === "array") {
      const collected = raw[name];
      if (Array.isArray(collected)) {
        collected.push(value);
      } else {
        raw[name] = [value];
      }
      continue;
    }
    raw[name] = value;
  }

  return raw;
}

/**
 * One CLI call as the argument layer sees it: the record a handler receives, and the names the
 * caller actually wrote. The two differ on purpose — `args` carries every declared default, so
 * recording its keys would make the supplied set a constant of the schema.
 */
export interface ParsedCliArgs {
  readonly args: ToolArgs;
  readonly supplied: readonly string[];
}

/**
 * argv for one tool, without the tool token and without the global flags, read against the same
 * manifest and handed to {@link parseArgs}. Every shell-level refusal is a `CliUsageError` so the
 * caller prints usage instead of a protocol error; the two schema refusals keep the message
 * `parseArgs` produced, so a CLI user reads what an agent would read.
 *
 * Only the parts a shell cannot express are lost: an array field collects repeats (`--paths a
 * --paths b`) and never splits a comma, because a path may contain one. `min`/`max` are left to
 * the handler, exactly as a protocol call leaves them.
 */
export function parseCliArgs(tool: string, input: InputLiteral, argv: readonly string[]): ToolArgs {
  return parseCliArgsAndNames(tool, input, argv).args;
}

/**
 * {@link parseCliArgs} plus the supplied names: `readArgv` only ever holds declared names, so its
 * keys are exactly the arguments whose source value was not `undefined`, before any default.
 */
export function parseCliArgsAndNames(
  tool: string,
  input: InputLiteral,
  argv: readonly string[],
): ParsedCliArgs {
  const raw = readArgv(tool, input, argv);
  try {
    return { args: parseArgs(tool, input, raw), supplied: Object.keys(raw).sort() };
  } catch (e) {
    if (e instanceof AppError) {
      throw new CliUsageError(e.message, usageOf(tool, input));
    }
    throw e;
  }
}

/**
 * The usage block for one tool, generated from its manifest so a new argument cannot ship
 * undocumented. The header names the positional a bare token binds; the last line names the flags
 * that belong to no tool and are therefore absent from every manifest's field list.
 */
export function usageOf(tool: string, input: InputLiteral): string {
  const positional = positionalFieldOf(input);
  const header =
    positional === undefined
      ? `sangkar ${tool} [options]`
      : `sangkar ${tool} [<${positional}>] [options]`;
  const fields = Object.entries(input).map(([name, field]) => {
    const marks = field.required === true ? " (required)" : "";
    const description = field.description === undefined ? "" : `  ${field.description}`;
    return `  --${name} <${field.type}>${marks}${description}`;
  });
  return [header, ...fields, "global options: --repo-root <path>, --data-dir <path>, --json"].join(
    "\n",
  );
}
