//! Whether an `index` run may start: the one place that decides it, so the answer is a pure
//! function of five facts and the ask-the-operator step is a single, injectable call.
//!
//! Ported from `src/shared/cli/index-gate.ts`. The tree is guessed by a walk
//! ([`sangkar_core::config::detect_tree`]), and a guess is what a `.sangkar` in `$HOME` or a
//! directory at the wrong end of a symlink punishes: the run reads a tree nobody chose, and —
//! without this gate — reports it only after the fact. The rule below never refuses a tree the
//! caller named, and never refuses the directory it is standing in; what it refuses is a tree found
//! *above* the working directory that nothing vouched for, when there is no one to ask.
//!
//! The five facts are `explicit` (was `--repo-root` given), `yes` (`--yes`), `levels` (how far the
//! walk climbed; `0` is the working directory itself), `tty` (can the process ask at all) and
//! `registered` (does the machine registry already name this tree). `via` — which marker stopped the
//! walk — is not one of them: the gate reads the root, never the reason it was found, which is why
//! nothing here needs `RepoDetection::via`.
//!
//! The question goes to **stderr** and the answer is read from **stdin**: stdout belongs to the
//! rendered envelope (`render_json`/`render_toon`), so a prompt on stdout would corrupt the bytes a
//! caller parses. That is a protocol requirement, not a style choice, and it is why every write
//! below goes through an injected [`Write`].

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use sangkar_core::config::{AppConfig, MAX_AUTODETECT_LEVELS};
use sangkar_core::registry::{canonical_root, read_registry};

use crate::render::EXIT_USAGE;

/// The question the gate asks when it can: one line, and `N` is what Enter means.
pub const CONTINUE_QUESTION: &str = "continue with this repo? (y/N) ";

/// What the process should do with an autodetected tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateAction {
    /// Nothing was guessed, the guess was confirmed, or the registry vouched for it.
    Run,
    /// A guess above the working directory, with an operator who can answer.
    Ask,
    /// A guess nothing vouched for and nobody could confirm.
    Refuse(RefuseReason),
}

/// Why a run was refused.
///
/// [`RefuseReason::Declined`] has no producer in [`index_gate`]: it is the operator answering no,
/// which [`confirm_tree`] records after [`ask_continue`] returns `false`. It shares the enum because
/// [`refusal_message`] spells all three, exactly as the TypeScript's `RefusalInput.reason` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefuseReason {
    /// No registry entry names the tree, and there was no terminal to ask.
    Unregistered,
    /// The tree is registered but sits further above the working directory than the walk budget.
    TooDeep,
    /// The operator was asked and did not say yes (or could not be reached).
    Declined,
}

/// The facts the gate reads, plus the two paths its refusal names.
///
/// The predicate reads five fields (`explicit`, `yes`, `levels`, `tty`, `registered`); `root` and
/// `cwd` are carried so [`refusal_message`] can name what was refused and where the caller stood
/// without the caller reassembling them. Every field is injected — the terminal, the registry and
/// the working directory are read by the composition root — so a test states a machine instead of
/// depending on the one it runs on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateInput {
    /// `--repo-root` named the tree, so nothing was guessed.
    pub explicit: bool,
    /// `--yes`: the operator states that the guessed tree is the one they mean.
    pub yes: bool,
    /// Directories between the working directory and the tree; `0` is the working directory itself.
    pub levels: usize,
    /// Whether the process can ask at all: a TTY on stdin.
    pub tty: bool,
    /// Whether the machine registry already names this tree.
    pub registered: bool,
    /// The tree the walk settled on, as the refusal spells it.
    pub root: PathBuf,
    /// The directory the caller is standing in, as the refusal spells it.
    pub cwd: PathBuf,
}

/// The decision, in the order the reasons win, exactly as the TypeScript version orders them:
///
/// 1. a named tree, or `--yes` — the operator spoke, so nothing is asked and nothing refused;
/// 2. `levels == 0` — the working directory is the tree, which is a first index, not a guess;
/// 3. a TTY — ask, because a guess above the working directory deserves a question;
/// 4. no TTY, tree registered and within [`MAX_AUTODETECT_LEVELS`] — proceed, because the registry
///    is a prior statement of intent and a script must keep working;
/// 5. otherwise refuse: unregistered, or deeper than the walk is allowed to reach unattended.
///
/// `--yes` is therefore read *before* any refusal exists: a fact that makes the gate run leaves no
/// refusal for a later step to override. What cannot be overridden is a refusal the gate has
/// already made — [`confirm_tree`] executes it, and never asks for a `y` that would undo it.
#[must_use]
pub fn index_gate(input: &GateInput) -> GateAction {
    if input.explicit || input.yes {
        return GateAction::Run;
    }
    if input.levels == 0 {
        return GateAction::Run;
    }
    if input.tty {
        return GateAction::Ask;
    }
    if input.registered && input.levels <= MAX_AUTODETECT_LEVELS {
        return GateAction::Run;
    }
    GateAction::Refuse(if input.registered {
        RefuseReason::TooDeep
    } else {
        RefuseReason::Unregistered
    })
}

/// The three lines a refusal writes to stderr: what was refused, and the ways to say it anyway. It
/// names the directory the caller is in as well as the one that was refused, because the fix is
/// almost always `--repo-root .` — the operator meant the tree they were standing in.
#[must_use]
pub fn refusal_message(input: &GateInput, reason: RefuseReason) -> String {
    let root = input.root.display().to_string();
    let why = match reason {
        RefuseReason::TooDeep => format!(
            "{root} is {} levels above {} (max {MAX_AUTODETECT_LEVELS})",
            input.levels,
            input.cwd.display()
        ),
        RefuseReason::Declined => format!("{root} was not confirmed"),
        RefuseReason::Unregistered => format!("{root} is not registered as a repository"),
    };
    [
        format!("refused: {why}. Nothing was indexed."),
        "  run `sangkar index --repo-root .` to index the directory you are in,".to_owned(),
        "  or `sangkar repo add <path>` to register the tree you meant,".to_owned(),
        format!("  or pass `--yes` if {root} is really that tree."),
    ]
    .join("\n")
}

/// One yes/no line. Only `y` and `yes` say yes, case-insensitively and after trimming the line the
/// terminal delivered; Enter, anything else, and a stdin that closes without an answer all say no,
/// so a prompt that cannot be answered can never approve a run.
///
/// The question is written to `output` — stderr in production — and the answer is read from
/// `input`. Both are injected, so the test supplies a [`std::io::Cursor`] and a `Vec<u8>` and the
/// production caller supplies the real stderr and stdin. A sink that cannot take the question cannot
/// take a `y` either, so a failed write is the same no a closed stdin is.
#[must_use]
pub fn ask_continue(input: &mut impl BufRead, output: &mut impl Write) -> bool {
    if output
        .write_all(CONTINUE_QUESTION.as_bytes())
        .and_then(|()| output.flush())
        .is_err()
    {
        return false;
    }
    let mut answer = String::new();
    match input.read_line(&mut answer) {
        // `Ok(0)` is a stdin that closed without a line, which is the TypeScript's `close` event;
        // a read error is the same "nobody answered". Both are a no, never a default yes.
        Ok(0) | Err(_) => false,
        // `String::trim` is the TypeScript's `answer.trim()` before `/^y(es)?$/i`, so ` y ` counts
        // and `yeah` does not.
        Ok(_) => {
            let answer = answer.trim();
            answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes")
        }
    }
}

/// The `index` verb's decision, its question and its refusal in the order they happen.
///
/// `None` means the caller may index. `Some(code)` means [`refusal_message`] has been written to
/// `stderr` (a newline after it) and the process stops with `code` — [`EXIT_USAGE`], the argv-and-
/// config refusal code, which is what the TypeScript `confirmTree` returns. No byte ever reaches
/// stdout: the question, the refusal and the answer all stay on the two streams that are not the
/// protocol's.
pub fn confirm_tree(
    input: &GateInput,
    stdin: &mut impl BufRead,
    stderr: &mut impl Write,
) -> Option<i32> {
    let reason = match index_gate(input) {
        GateAction::Run => return None,
        GateAction::Refuse(reason) => reason,
        GateAction::Ask => {
            if ask_continue(stdin, stderr) {
                return None;
            }
            RefuseReason::Declined
        }
    };
    // The exit code carries the refusal even when the diagnostic cannot be written: a broken (or
    // closed) stderr must not turn a refusal into a run.
    let _ = writeln!(stderr, "{}", refusal_message(input, reason));
    Some(EXIT_USAGE)
}

/// The five facts for this process, read from the config the composition root built.
///
/// `yes` is `--yes` from the verb arguments and `tty` is `std::io::stdin().is_terminal()` taken at
/// the call site: both are injected rather than read here, so the gate itself stays pure and a test
/// never depends on the terminal it happens to run under.
#[must_use]
pub fn gate_input(config: &AppConfig, cwd: &Path, yes: bool, tty: bool) -> GateInput {
    GateInput {
        explicit: config.repo_detection.explicit,
        yes,
        levels: config.repo_detection.levels,
        tty,
        registered: is_registered(config),
        root: config.repo_root.clone(),
        cwd: cwd.to_path_buf(),
    }
}

/// Whether the machine registry already names this tree, compared on the canonical root.
///
/// The registry holds canonical roots, so [`canonical_root`] is applied to the tree before the
/// lookup: the same tree spelled `./` or through a symlink is the same entry. A registry that cannot
/// be read is an empty one ([`read_registry`]), which is the unregistered answer — the gate never
/// treats a broken file as a prior statement of intent.
#[must_use]
pub fn is_registered(config: &AppConfig) -> bool {
    let root = canonical_root(&config.repo_root, Some(&config.home));
    read_registry(&config.registry_path)
        .repos
        .iter()
        .any(|entry| entry == &root)
}
