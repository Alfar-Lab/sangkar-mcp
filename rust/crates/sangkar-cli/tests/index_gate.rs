//! The index gate (`src/shared/cli/index-gate.ts`): the decision as a truth table over its five
//! facts, the one line the prompt reads, the three lines a refusal writes, and the exit code.
//!
//! Ported from `tests/cli-index-gate.test.ts`. The TypeScript suite's end-to-end half spawns the
//! CLI; the verbs land with the dispatch task, so the last groups here drive the same path through
//! [`confirm_tree`] with both streams injected — the question, the answer, and the exact bytes that
//! reach stderr — and through [`gate_input`] against a config built in a temporary home. Nothing in
//! this file touches the real terminal, this repository's `.sangkar/`, or `~/.config/sangkar/`:
//! `SANGKAR_HOME` is always a temporary directory, and every registry entry is written there.

use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_cli::args::{Verb, parse_verb_args};
use sangkar_cli::index_gate::{
    CONTINUE_QUESTION, GateAction, GateInput, RefuseReason, ask_continue, confirm_tree, gate_input,
    index_gate, is_registered, refusal_message,
};
use sangkar_cli::render::EXIT_USAGE;
use sangkar_core::config::{AppConfig, load_config};
use sangkar_core::registry::register_repo;

const RUN: GateAction = GateAction::Run;
const ASK: GateAction = GateAction::Ask;
const UNREGISTERED: GateAction = GateAction::Refuse(RefuseReason::Unregistered);
const TOO_DEEP: GateAction = GateAction::Refuse(RefuseReason::TooDeep);

/// The row every case starts from: an autodetected, unregistered tree two levels above the working
/// directory, with no terminal and no `--yes`.
fn base() -> GateInput {
    GateInput {
        explicit: false,
        yes: false,
        levels: 2,
        tty: false,
        registered: false,
        root: PathBuf::from("/home/someone/work/app"),
        cwd: PathBuf::from("/home/someone/work/app/a/b"),
    }
}

#[test]
fn the_gate_is_a_truth_table_over_its_five_facts() {
    // explicit, yes, levels, tty, registered → the action it must produce. Depth 9 stands for "far
    // above the walk budget", which is where a fact combination's branches are all visible.
    #[rustfmt::skip]
    const TABLE: &[(bool, bool, usize, bool, bool, GateAction)] = &[
        // explicit yes   levels tty   registered
        (false,   false, 9,     false, false, UNREGISTERED),
        (false,   false, 9,     false, true,  TOO_DEEP),
        (false,   false, 9,     true,  false, ASK),
        (false,   false, 9,     true,  true,  ASK),
        // `--yes` is the operator answering before the question exists.
        (false,   true,  9,     false, false, RUN),
        (false,   true,  9,     false, true,  RUN),
        (false,   true,  9,     true,  false, RUN),
        (false,   true,  9,     true,  true,  RUN),
        // A named tree is never guessed, so it is never questioned and never refused.
        (true,    false, 9,     false, false, RUN),
        (true,    false, 9,     false, true,  RUN),
        (true,    false, 9,     true,  false, RUN),
        (true,    false, 9,     true,  true,  RUN),
        (true,    true,  9,     false, false, RUN),
        (true,    true,  9,     false, true,  RUN),
        (true,    true,  9,     true,  false, RUN),
        (true,    true,  9,     true,  true,  RUN),
        // The working directory itself is the tree: a bootstrap, not a guess, at any depth of fact.
        (false,   false, 0,     false, false, RUN),
        (false,   false, 0,     false, true,  RUN),
        (false,   false, 0,     true,  false, RUN),
        (false,   false, 0,     true,  true,  RUN),
        // The walk budget is inclusive: 3 levels is inside it, 4 is not.
        (false,   false, 1,     false, true,  RUN),
        (false,   false, 3,     false, true,  RUN),
        (false,   false, 4,     false, true,  TOO_DEEP),
        // An unregistered tree is refused without a terminal whatever its depth.
        (false,   false, 1,     false, false, UNREGISTERED),
        (false,   false, 4,     false, false, UNREGISTERED),
    ];

    for &(explicit, yes, levels, tty, registered, expected) in TABLE {
        let input = GateInput {
            explicit,
            yes,
            levels,
            tty,
            registered,
            ..base()
        };
        assert_eq!(
            index_gate(&input),
            expected,
            "explicit={explicit} yes={yes} levels={levels} tty={tty} registered={registered}"
        );
    }
}

#[test]
fn a_named_tree_is_never_questioned_and_never_refused() {
    let input = GateInput {
        explicit: true,
        levels: 9,
        ..base()
    };
    assert_eq!(index_gate(&input), RUN);
}

#[test]
fn yes_is_the_operators_answer_so_it_wins_over_every_refusal() {
    let deep = GateInput {
        yes: true,
        levels: 9,
        ..base()
    };
    assert_eq!(index_gate(&deep), RUN);
    let deep_registered = GateInput {
        registered: true,
        ..deep
    };
    assert_eq!(index_gate(&deep_registered), RUN);
}

#[test]
fn levels_zero_is_the_working_directory_so_it_is_a_bootstrap() {
    assert_eq!(
        index_gate(&GateInput {
            levels: 0,
            ..base()
        }),
        RUN
    );
    assert_eq!(
        index_gate(&GateInput {
            levels: 0,
            tty: true,
            ..base()
        }),
        RUN
    );
}

#[test]
fn a_tty_is_asked_registered_or_not_at_any_depth_above_the_working_directory() {
    assert_eq!(
        index_gate(&GateInput {
            tty: true,
            ..base()
        }),
        ASK
    );
    let registered_and_deep = GateInput {
        tty: true,
        registered: true,
        levels: 4,
        ..base()
    };
    assert_eq!(index_gate(&registered_and_deep), ASK);
}

#[test]
fn no_tty_a_registered_tree_within_the_walk_budget_proceeds() {
    assert_eq!(
        index_gate(&GateInput {
            registered: true,
            levels: 1,
            ..base()
        }),
        RUN
    );
    assert_eq!(
        index_gate(&GateInput {
            registered: true,
            levels: 3,
            ..base()
        }),
        RUN
    );
}

#[test]
fn no_tty_a_registered_tree_beyond_the_budget_is_refused_as_too_deep() {
    let input = GateInput {
        registered: true,
        levels: 4,
        ..base()
    };
    assert_eq!(index_gate(&input), TOO_DEEP);
}

#[test]
fn no_tty_an_unregistered_tree_is_refused_whatever_its_depth() {
    for levels in [1, 2, 4, 9] {
        assert_eq!(index_gate(&GateInput { levels, ..base() }), UNREGISTERED);
    }
}

#[test]
fn yes_runs_an_ask_case_and_asks_nothing() {
    let asked = GateInput {
        tty: true,
        ..base()
    };
    assert_eq!(index_gate(&asked), ASK);
    let answered = GateInput { yes: true, ..asked };
    assert_eq!(index_gate(&answered), RUN);

    // The question was answered before it was asked: no byte is written and stdin is never read.
    let mut stdin = Cursor::new("n\n");
    let mut stderr: Vec<u8> = Vec::new();
    assert_eq!(confirm_tree(&answered, &mut stdin, &mut stderr), None);
    assert!(
        stderr.is_empty(),
        "a --yes run writes nothing: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(stdin.position(), 0);
}

#[test]
fn a_refusal_is_never_overridden_and_never_asked_about() {
    let refused = GateInput { ..base() };
    assert_eq!(index_gate(&refused), UNREGISTERED);

    // A `y` waiting on stdin is not consent for a question this run never asked: the refusal is
    // executed, the message goes to stderr, the exit code is the usage code, and stdin is untouched.
    let mut stdin = Cursor::new("y\n");
    let mut stderr: Vec<u8> = Vec::new();
    assert_eq!(
        confirm_tree(&refused, &mut stdin, &mut stderr),
        Some(EXIT_USAGE)
    );
    assert_eq!(EXIT_USAGE, 2);

    let written = String::from_utf8(stderr).expect("stderr is UTF-8");
    assert_eq!(
        written,
        format!(
            "{}\n",
            refusal_message(&refused, RefuseReason::Unregistered)
        )
    );
    assert!(!written.contains(CONTINUE_QUESTION));
    assert_eq!(stdin.position(), 0);
}

#[test]
fn an_answered_question_proceeds_and_writes_only_the_question() {
    let asked = GateInput {
        tty: true,
        ..base()
    };
    let mut stdin = Cursor::new("y\n");
    let mut stderr: Vec<u8> = Vec::new();
    assert_eq!(confirm_tree(&asked, &mut stdin, &mut stderr), None);
    assert_eq!(
        String::from_utf8(stderr).expect("stderr is UTF-8"),
        CONTINUE_QUESTION
    );
}

#[test]
fn a_declined_answer_exits_two_with_the_declined_message() {
    let asked = GateInput {
        tty: true,
        ..base()
    };
    let mut stdin = Cursor::new("n\n");
    let mut stderr: Vec<u8> = Vec::new();
    assert_eq!(
        confirm_tree(&asked, &mut stdin, &mut stderr),
        Some(EXIT_USAGE)
    );

    let written = String::from_utf8(stderr).expect("stderr is UTF-8");
    assert!(written.starts_with(CONTINUE_QUESTION));
    assert_eq!(
        written
            .strip_prefix(CONTINUE_QUESTION)
            .expect("the question comes first"),
        format!("{}\n", refusal_message(&asked, RefuseReason::Declined))
    );
    assert!(written.contains("/home/someone/work/app was not confirmed"));
}

/// One line on an injected stdin, with the question captured from the injected stderr.
fn answer(line: &str) -> (bool, String) {
    let mut stdin = Cursor::new(line);
    let mut stderr: Vec<u8> = Vec::new();
    let taken = ask_continue(&mut stdin, &mut stderr);
    (
        taken,
        String::from_utf8(stderr).expect("the question is UTF-8"),
    )
}

#[test]
fn the_question_is_written_byte_for_byte_and_only_the_question() {
    let (_, written) = answer("\n");
    assert_eq!(written, CONTINUE_QUESTION);
    assert!(CONTINUE_QUESTION.ends_with("(y/N) "));
}

#[test]
fn only_y_and_yes_say_yes() {
    for line in [
        "y\n", "yes\n", "Y\n", "YES\n", "Yes\n", "y", "yes", " y \n", "yes\r\n",
    ] {
        assert!(answer(line).0, "{line:?} must proceed");
    }
}

#[test]
fn enter_anything_else_and_a_stdin_that_closes_without_an_answer_are_all_no() {
    for line in [
        "\n",
        "n\n",
        "N\n",
        "no\n",
        "yep\n",
        "yeah\n",
        "yes please\n",
        "yess\n",
        " n \n",
        "",
    ] {
        assert!(!answer(line).0, "{line:?} must not proceed");
    }
}

/// An stderr that cannot take the question.
struct BrokenSink;

impl Write for BrokenSink {
    fn write(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "stderr is gone",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_question_that_cannot_be_written_is_a_no() {
    let mut stdin = Cursor::new("y\n");
    assert!(!ask_continue(&mut stdin, &mut BrokenSink));
}

#[test]
fn the_refusal_names_what_was_refused_the_working_directory_and_three_ways_forward() {
    let input = GateInput {
        levels: 4,
        root: PathBuf::from("/home/someone"),
        cwd: PathBuf::from("/home/someone/work/app"),
        ..base()
    };
    let message = refusal_message(&input, RefuseReason::TooDeep);
    assert!(message.contains("/home/someone is 4 levels above /home/someone/work/app (max 3)"));
    assert!(message.contains("Nothing was indexed."));
    assert!(message.contains("--repo-root ."));
    assert!(message.contains("sangkar repo add <path>"));
    assert!(message.contains("--yes"));
}

#[test]
fn an_unregistered_tree_says_so_and_a_declined_one_does_not_argue() {
    let input = GateInput {
        levels: 1,
        root: PathBuf::from("/r"),
        cwd: PathBuf::from("/r/x"),
        ..base()
    };
    assert!(
        refusal_message(&input, RefuseReason::Unregistered)
            .contains("/r is not registered as a repository")
    );
    assert!(refusal_message(&input, RefuseReason::Declined).contains("/r was not confirmed"));
}

#[test]
fn the_refusal_text_is_verbatim() {
    let input = GateInput {
        levels: 4,
        root: PathBuf::from("/home/someone"),
        cwd: PathBuf::from("/home/someone/work/app"),
        ..base()
    };
    assert_eq!(
        refusal_message(&input, RefuseReason::TooDeep),
        "refused: /home/someone is 4 levels above /home/someone/work/app (max 3). Nothing was indexed.\n  \
         run `sangkar index --repo-root .` to index the directory you are in,\n  \
         or `sangkar repo add <path>` to register the tree you meant,\n  \
         or pass `--yes` if /home/someone is really that tree."
    );
}

/// A temporary directory that removes itself, so the suite leaves no tree or registry behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-gate-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// `self.join…` spelled out, because one component here is a nested path.
    fn under(&self, nested: &str) -> PathBuf {
        self.0.join(nested)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The config for one call, with `SANGKAR_HOME` stated rather than inherited: the real
/// `~/.config/sangkar/` is never read and never written.
fn config_for(args: &[&str], home: &Path, cwd: &Path) -> AppConfig {
    let argv: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
    let env = |name: &str| (name == "SANGKAR_HOME").then(|| home.display().to_string());
    load_config(&argv, &env, cwd).expect("the config resolves")
}

#[test]
fn the_facts_come_from_the_walk_and_the_registry() {
    let home = TempDir::new("home");
    let tree = TempDir::new("tree");
    fs::create_dir_all(tree.under(".git")).expect("create the marker");
    let cwd = tree.under("a/b");
    fs::create_dir_all(&cwd).expect("create the nested directory");

    // A walk that climbed two levels and found a `.git` the gate never asks about: `levels` is the
    // fact, and `via` is only reported.
    let config = config_for(&["index"], home.path(), &cwd);
    assert_eq!(config.repo_detection.levels, 2);
    assert!(!config.repo_detection.explicit);

    let facts = gate_input(&config, &cwd, false, false);
    assert!(!facts.explicit);
    assert!(!facts.yes);
    assert!(!facts.tty);
    assert_eq!(facts.levels, 2);
    assert_eq!(facts.root, tree.path());
    assert_eq!(facts.cwd, cwd);
    assert!(!facts.registered);
    assert_eq!(index_gate(&facts), UNREGISTERED);

    // The lookup is on the canonical root, so registering the tree flips the fact and a script
    // within the budget is let through with no question.
    let registered = register_repo(&config.registry_path, tree.path());
    assert!(registered.added);
    assert!(is_registered(&config));
    let facts = gate_input(&config, &cwd, false, false);
    assert!(facts.registered);
    assert_eq!(index_gate(&facts), RUN);

    // A sibling tree in the same machine home is not the tree that was registered.
    let other = TempDir::new("other");
    let other_config = config_for(&["index"], home.path(), other.path());
    assert!(!is_registered(&other_config));
}

#[test]
fn a_named_tree_arrives_explicit_at_level_zero() {
    let home = TempDir::new("home");
    let tree = TempDir::new("tree");
    fs::create_dir_all(tree.under("a/b")).expect("create the nested directory");
    let cwd = tree.under("a/b");

    let config = config_for(&["index", "--repo-root", "."], home.path(), &cwd);
    let facts = gate_input(&config, &cwd, false, false);
    assert!(facts.explicit);
    assert_eq!(facts.levels, 0);
    assert_eq!(facts.root, cwd);
    assert_eq!(index_gate(&facts), RUN);
}

#[test]
fn yes_from_the_index_flag_is_the_fact_the_gate_reads() {
    let home = TempDir::new("home");
    let tree = TempDir::new("tree");
    fs::create_dir_all(tree.under(".git")).expect("create the marker");
    let cwd = tree.under("a/b");
    fs::create_dir_all(&cwd).expect("create the nested directory");

    let argv: Vec<String> = ["--yes"].iter().map(|arg| (*arg).to_owned()).collect();
    let args = parse_verb_args(Verb::Index, &argv).expect("the index verb takes --yes");
    assert!(args.switch("yes"), "--yes is the index verb's own switch");

    let config = config_for(&["index"], home.path(), &cwd);
    let asked = gate_input(&config, &cwd, false, false);
    assert_eq!(index_gate(&asked), UNREGISTERED);
    let answered = gate_input(&config, &cwd, args.switch("yes"), false);
    assert!(answered.yes);
    assert_eq!(index_gate(&answered), RUN);
}
