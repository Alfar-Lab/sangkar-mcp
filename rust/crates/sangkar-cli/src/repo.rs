//! The `repo` verb: the machine registry and one tree's settings, on the command line.
//!
//! Ported from `src/shared/cli/repo.ts`. Two answers live here, and the split is the design:
//!
//! - `add`, `remove` and `list` are the registry's writers and its reader, in the one canonical
//!   spelling [`sangkar_core::registry`] uses — so the path a person types and the path the daemon
//!   polls on its next tick are the same string;
//! - `config` is the only place a tree's settings file is written from the command line. It reads
//!   the file it already has, overrides exactly the flags it was given, writes it, and prints what
//!   a re-read returns: the effective values the daemon will act on, not the text that was typed.
//!
//! A subcommand's path may sit anywhere among its own flags, so the path is the first token that is
//! not a flag — [`crate::args::parse_verb_args`] consumes each flag's value and never lets a `--`
//! token become a path. Taking the first token instead made `repo config --watch false .` write the
//! settings of a directory called `--watch` and answer the same three lines, exit 0: a silent wrong
//! target, which is the one failure a settings verb cannot afford.
//!
//! # The answer is returned, not printed
//!
//! Every line here is data a person or a script reads, and the workspace bans the print macros and
//! the stdout handle in `src/` (stdout is the MCP protocol everywhere else in this binary). So the
//! decision and the writing are two steps: `respond` composes the bytes and the code,
//! [`run_repo_cli`] writes them to the two handles its caller owns. A test passes two `Vec<u8>` and
//! reads them back, the binary passes the process's own handles, and no line is assembled twice — so
//! a test cannot miss a line the binary prints.
//!
//! The four global flags are accepted next to every subcommand. `--json` changes only `list`'s
//! answer; the rest are otherwise inert, because the registry lives in the machine home rather than
//! in a tree: a flag that would mean nothing is accepted and ignored, never refused.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sangkar_core::config::{self, Env};
use sangkar_core::global_config;
use sangkar_core::registry::{self, REGISTRY_VERSION};
use sangkar_core::repo_config::{self, RepoConfig, WATCH_INTERVAL_MAX_MS, WATCH_INTERVAL_MIN_MS};

use crate::args::{self, Verb};
use crate::render::{EXIT_FAILURE, EXIT_USAGE};

/// The registry's file name inside the machine home. `config::load_config` composes the same pair
/// into `AppConfig::registry_path`; this verb reads the machine home itself because it needs
/// nothing else a process configuration holds — and building one would demand a cursor key and a
/// resolvable executable that a registry write has no use for. `tests/repo_cmd.rs` pins the two
/// compositions to the same file, so the name cannot drift here.
const REGISTRY_FILE_NAME: &str = "sangkar.json";

/// One `sangkar repo` invocation: the tokens *after* the verb name (`sangkar repo` already removed,
/// which is the tail [`crate::args::resolve_command`] answers with and what the TypeScript's
/// `runRepoCli(verbArgv)` received), the machine's environment and working directory, and the two
/// streams the caller owns. Answers the code the process exits with.
///
/// The environment and the working directory are injected for the same reason
/// [`sangkar_core::config`] injects its own lookup: a test states a machine — a temporary home, a
/// temporary tree — instead of editing the environment and working directory of the process it
/// shares with every other test.
pub fn run_repo_cli(
    argv: &[String],
    env: Env<'_>,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let answer = respond(argv, env, cwd);
    match write_answer(out, err, &answer) {
        Ok(()) => answer.code,
        // A stream that cannot be written is a failed run rather than a silent one: the answer
        // never reached the caller, and the code says so.
        Err(error) => {
            let _ = writeln!(err, "sangkar repo: could not write the answer: {error}");
            EXIT_FAILURE
        }
    }
}

/// The whole answer, once per stream: a caller capturing stdout gets complete lines, never a
/// half-written one.
fn write_answer(out: &mut dyn Write, err: &mut dyn Write, answer: &Answer) -> io::Result<()> {
    out.write_all(answer.stdout.as_bytes())?;
    out.flush()?;
    err.write_all(answer.stderr.as_bytes())?;
    err.flush()
}

/// What one invocation answers: the bytes each stream receives and the code that goes with them.
#[derive(Debug)]
struct Answer {
    /// What the caller reads: the registry, or one tree's effective settings.
    stdout: String,
    /// The refusal or the failure; a usage refusal follows its reason with the block.
    stderr: String,
    /// `0`, [`EXIT_FAILURE`] or [`EXIT_USAGE`].
    code: i32,
}

impl Answer {
    /// A run that answered: the text for stdout and the code a successful run earns.
    fn printed(stdout: String) -> Self {
        Self {
            stdout,
            stderr: String::new(),
            code: 0,
        }
    }
}

/// The decision, without a stream: what each surface gets and the code that goes with it.
fn respond(argv: &[String], env: Env<'_>, cwd: &Path) -> Answer {
    let usage = args::verb_spec(Verb::Repo).usage;
    let globals = match args::split_globals(argv) {
        Ok(globals) => globals,
        // `split_globals` refuses with the tool-level block; a verb re-points the same refusal at
        // its own block, which is what every TypeScript verb does before dispatching.
        Err(error) => return refuse(&error.message, usage),
    };
    // `--help` is answered before anything is validated, exactly as the TypeScript verb answers it:
    // asking for the shape of the command must not require satisfying it first, so `repo add
    // --help` is a help request and not a broken `repo add`.
    if globals.help {
        return Answer::printed(format!("{usage}\n"));
    }
    let verb_args = match args::parse_verb_args(Verb::Repo, &globals.rest) {
        Ok(verb_args) => verb_args,
        Err(error) => return refuse(&error.message, usage),
    };

    let home = home_of(env);
    let registry_path = home.join(REGISTRY_FILE_NAME);
    // The machine-level file, read once: it is what a tree inherits before its own settings file
    // speaks, so it is the fallback `config` writes over rather than the tree's own current values.
    let repo_defaults = global_config::repo_defaults_of(&global_config::read_global_config(&home));

    match verb_args.subcommand {
        Some("add") => {
            let Some(raw) = verb_args.positional().filter(|raw| !raw.is_empty()) else {
                return refuse("repo add needs a path", usage);
            };
            let root = canonical(raw, cwd, &home);
            let result = registry::register_repo(&registry_path, &root);
            if result.added {
                return Answer::printed(registry_line("registered", &root));
            }
            // `register_repo` swallows a failed write and answers `added: false` for it, so the
            // registry it hands back is what separates "was already there" from "could not be
            // written".
            if result.registry.repos.contains(&root) {
                return Answer::printed(registry_line("already registered", &root));
            }
            fail(&format!("could not write {}", registry_path.display()))
        }

        Some("remove") => {
            let Some(raw) = verb_args.positional().filter(|raw| !raw.is_empty()) else {
                return refuse("repo remove needs a path", usage);
            };
            let root = canonical(raw, cwd, &home);
            let result = registry::remove_repo(&registry_path, &root);
            if result.removed {
                return Answer::printed(registry_line("removed", &root));
            }
            // The same split as `add`: still listed means the write failed, absent means it was
            // never there.
            if result.registry.repos.contains(&root) {
                return fail(&format!("could not write {}", registry_path.display()));
            }
            // A removal that removed nothing is still an answer worth printing; the code is what
            // tells a script the entry was not there.
            Answer {
                code: EXIT_FAILURE,
                ..Answer::printed(registry_line("not registered", &root))
            }
        }

        Some("list") => {
            let registry = registry::read_registry(&registry_path);
            // `--json` was stripped from the rest by `split_globals`, so the switch is the only
            // record of it.
            if globals.json {
                return match serde_json::to_string(&registry_document(&registry)) {
                    Ok(document) => Answer::printed(format!("{document}\n")),
                    // A document of one number and a list of strings cannot fail to serialise; the
                    // case is answered anyway, because a panic in a verb is worse than a refusal.
                    Err(error) => fail(&format!(
                        "the registry could not be rendered as JSON: {error}"
                    )),
                };
            }
            // Nothing to say about an empty registry is the whole answer: one path per line, no
            // header.
            let mut lines = String::new();
            for repo in &registry.repos {
                lines.push_str(&repo.to_string_lossy());
                lines.push('\n');
            }
            Answer::printed(lines)
        }

        Some("config") => {
            // The flags are read before the path, so a malformed flag answers for itself rather
            // than as a missing path: `repo config --watch` is a broken flag, not a path someone
            // forgot to type. The spelling of the value was already checked by `parse_verb_args`,
            // and so was the rule that these three belong to `config` alone.
            let watch = verb_args.boolean("watch");
            let telemetry = verb_args.boolean("telemetry");
            let interval = match verb_args.integer("watch-interval") {
                None => None,
                Some(ms) => match u64::try_from(ms)
                    .ok()
                    .filter(|ms| (WATCH_INTERVAL_MIN_MS..=WATCH_INTERVAL_MAX_MS).contains(ms))
                {
                    Some(within) => Some(within),
                    // Refused rather than written and clamped: the file would say one thing and the
                    // daemon do another, which is the silent divergence a settings file exists to
                    // prevent. The band comes from `repo_config`, never from a number typed here.
                    None => {
                        return refuse(
                            &format!(
                                "--watch-interval must be {WATCH_INTERVAL_MIN_MS}..{WATCH_INTERVAL_MAX_MS} ms, not {ms}"
                            ),
                            usage,
                        );
                    }
                },
            };
            let root = match verb_args.positional().filter(|raw| !raw.is_empty()) {
                Some(raw) => canonical(raw, cwd, &home),
                // No path is refused, not autodetected. `src/shared/cli/repo.ts:178-181` says
                // `repo config needs a path` for exactly this case, and the reason is the write:
                // autodetecting here would let `repo config --watch false` silently rewrite the
                // settings of whatever tree the caller happened to be standing in, which is a
                // surprise an operator cannot see afterwards. `add`/`remove` refuse a missing path
                // for the same kind of reason.
                None => return refuse("repo config needs a path", usage),
            };
            let current = repo_config::read_repo_config(&root, None, repo_defaults);
            let next = RepoConfig {
                watch: watch.unwrap_or(current.watch),
                watch_interval_ms: interval.unwrap_or(current.watch_interval_ms),
                telemetry: telemetry.unwrap_or(current.telemetry),
                ..current
            };
            if let Err(error) = repo_config::write_repo_config(&next) {
                return fail(&format!(
                    "could not write the settings for {}: {error}",
                    root.display()
                ));
            }
            // Re-read rather than echo the arguments: the file is the contract, and a value the
            // reader clamps is reported as the daemon will use it.
            let effective = repo_config::read_repo_config(&root, None, repo_defaults);
            Answer::printed(format!(
                "watch={}\ninterval={}ms\ntelemetry={}\n",
                on_off(effective.watch),
                effective.watch_interval_ms,
                on_off(effective.telemetry),
            ))
        }

        Some(other) => refuse(&format!("unknown subcommand \"{other}\""), usage),
        // `parse_verb_args` refuses a missing subcommand before this point; the arm exists so no
        // future subcommand list can silently fall through to doing nothing.
        None => refuse("repo needs a subcommand", usage),
    }
}

/// The registry as the document `list --json` prints and the file on disk holds:
/// `{"version":1,"repos":[...]}` in that key order and no trailing newline.
///
/// The shape is the registry module's, not this verb's: [`REGISTRY_VERSION`] is its constant and
/// `registry::write_registry` writes the same two keys in the same order. `Registry` carries no
/// `Serialize` and the struct that module writes is private to it, so the document is declared here
/// — with `preserve_order` keeping the order written below — and `tests/repo_cmd.rs` pins this
/// rendering byte for byte to the body of the file `write_registry` produces, so the printed
/// document and the stored one cannot drift apart.
fn registry_document(registry: &registry::Registry) -> serde_json::Value {
    let repos: Vec<String> = registry
        .repos
        .iter()
        .map(|repo| repo.to_string_lossy().into_owned())
        .collect();
    serde_json::json!({ "version": REGISTRY_VERSION, "repos": repos })
}

/// One registry line: what happened, the canonical path, then a newline. All four of `add` and
/// `remove`'s answers have this shape.
fn registry_line(what: &str, root: &Path) -> String {
    format!("{what} {}\n", root.display())
}

/// `~/` expands against `home`, everything else resolves against the directory the verb was run in
/// — a shell's relative path is the caller's intent rather than a string to guess at — and
/// [`registry::canonical_root`] then applies the registry's own spelling, so the path printed and
/// the path stored are one string.
fn canonical(raw: &str, cwd: &Path, home: &Path) -> PathBuf {
    let expanded = config::expand_home(raw, home).unwrap_or_else(|| PathBuf::from(raw));
    // The tilde was expanded above against the injected home, so the module's own expansion has
    // nothing left to do and the process's `$HOME` is never the base here.
    registry::canonical_root(&config::resolve_path(cwd, &expanded), None)
}

/// `$HOME` as the injected lookup states it: the base a leading `~/` expands against, which is what
/// the TypeScript's `homedir()` read. An empty variable is an absent one, and an absent one takes
/// the process's home, which [`sangkar_core::config`] owns.
fn home_of(env: Env<'_>) -> PathBuf {
    env("HOME")
        .filter(|home| !home.is_empty())
        .map_or_else(config::process_home_dir, PathBuf::from)
}

/// `on`/`off`, the only spelling the settings lines use.
fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// The reason first, then the block, so the last thing a caller sees is how to call it right.
fn refuse(reason: &str, usage: &str) -> Answer {
    Answer {
        stdout: String::new(),
        stderr: format!("{reason}\n{usage}\n"),
        code: EXIT_USAGE,
    }
}

/// A failure with no usage to offer: the verb was called correctly and the filesystem refused.
fn fail(reason: &str) -> Answer {
    Answer {
        stdout: String::new(),
        stderr: format!("sangkar repo: {reason}\n"),
        code: EXIT_FAILURE,
    }
}
