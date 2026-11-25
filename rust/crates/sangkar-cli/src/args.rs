//! The command line's argument layer: one parser per tool, driven by the same field descriptors the
//! MCP surface advertises, plus one flag table for the verbs.
//!
//! Ported from `src/shared/cli/args.ts`. [`parse_args`], [`matches_type`] and [`expectation_of`] are
//! the MCP-side validator moved unchanged: a `sangkar <tool>` invocation and a tool call have to
//! accept and refuse exactly the same arguments, and a second, laxer schema on the CLI side is the
//! drift this module exists to prevent. What is new here is only the argv reading —
//! [`split_globals`] takes the flags no tool declares, [`parse_cli_args`] turns the remaining tokens
//! into the record [`parse_args`] already knows how to check — plus the verb table.
//!
//! The verb table is data ([`VERB_SPECS`]) rather than code because a verb refuses a flag that is
//! not in its set: the set has to be something a test can enumerate. It covers every flag the
//! TypeScript CLI accepts today — `worker --mode/--force`, `index` plus `--yes` and
//! `--no-register`, `telemetry --out/--serve/--port/--host/--since`, `daemon`'s subcommands with
//! `--port`/`--host` on `run` only, and `repo`'s subcommands with `--watch`, `--watch-interval` and
//! `--telemetry` on `config` — and every verb also takes the four globals, which
//! [`split_globals`] removes before the table is consulted.
//!
//! The tool descriptors are the contract the seven manifest crates fill in: [`InputField`] carries
//! a field's type, requirement, default, bounds, allowed values and item type, and [`InputLiteral`]
//! keeps them in declaration order, because the usage block and `tools/list` both publish that
//! order.

use std::collections::BTreeMap;

use sangkar_core::errors::AppError;
use serde_json::{Map, Value};

/// The flags that sit before the tool token and belong to no tool's schema, so no manifest can
/// declare or refuse them. `--json` and `--help` are switches; these two carry a value.
pub const GLOBAL_VALUE_FLAGS: [&str; 2] = ["repo-root", "data-dir"];

/// What a refusal from [`split_globals`] points at, because no tool is known yet.
pub const GLOBAL_USAGE: &str =
    "sangkar [--repo-root <path>] [--data-dir <path>] [--json] [--help] <tool> [arguments]";

/// The bound `expectation_of` reports for an integer with no declared `min`; `i64::MIN` would put
/// an implementation limit in a user-facing hint, so the JavaScript safe-integer bound is kept.
const MIN_SAFE_INTEGER: i64 = -9_007_199_254_740_991;

/// The ceiling of the same pair.
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// The argument map a validated call hands a handler: declared keys only, defaults applied, in
/// declaration order.
pub type ToolArgs = Map<String, Value>;

/// A command line the schema cannot accept, carrying the usage block the caller prints.
///
/// Separate from [`AppError`] because this failure is the argv, not the call: it never crosses the
/// MCP boundary, and its refusal is answered with a usage block rather than a retry hint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliUsageError {
    /// The one-line reason, printed before the block.
    pub message: String,
    /// The block the caller prints after it.
    pub usage: String,
}

impl CliUsageError {
    #[must_use]
    pub fn new(message: impl Into<String>, usage: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            usage: usage.into(),
        }
    }
}

impl std::fmt::Display for CliUsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliUsageError {}

/// One declared argument, exactly the literal a manifest exports (`TECH-SPEC.md` §6).
///
/// The TypeScript keeps `type` a plain string because one manifest writes its table without
/// `as const`. Here the type is an enum: a manifest field with a type the parser does not know was
/// a runtime refusal there and is a compile error here, which is the stronger half of that comment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputField {
    /// The declared type, spelled the way `tools/list` publishes it.
    pub kind: FieldType,
    /// Whether the call must supply it.
    pub required: bool,
    /// The value an absent argument takes; `None` means "absent is a fact", not "empty".
    pub default: Option<DefaultValue>,
    /// The reported lower bound of an integer, used by [`expectation_of`] only: `min`/`max` are the
    /// handler's business, and the parser neither clamps nor refuses.
    pub min: Option<i64>,
    /// The reported upper bound of an integer.
    pub max: Option<i64>,
    /// The closed value set of an enum.
    pub values: &'static [&'static str],
    /// The declared item type of an array; strings when the manifest declares none.
    pub items: Items,
    /// The sentence the usage block reproduces.
    pub description: Option<&'static str>,
}

impl InputField {
    /// A plain string field.
    #[must_use]
    pub const fn string() -> Self {
        Self {
            kind: FieldType::String,
            required: false,
            default: None,
            min: None,
            max: None,
            values: &[],
            items: Items::String,
            description: None,
        }
    }

    /// A whole-number field.
    #[must_use]
    pub const fn integer() -> Self {
        Self {
            kind: FieldType::Integer,
            ..Self::string()
        }
    }

    /// A true/false field.
    #[must_use]
    pub const fn boolean() -> Self {
        Self {
            kind: FieldType::Boolean,
            ..Self::string()
        }
    }

    /// One of a closed set of strings.
    #[must_use]
    pub const fn enumeration(values: &'static [&'static str]) -> Self {
        Self {
            kind: FieldType::Enum,
            values,
            ..Self::string()
        }
    }

    /// A list whose items are read in `items`.
    #[must_use]
    pub const fn array(items: Items) -> Self {
        Self {
            kind: FieldType::Array,
            items,
            ..Self::string()
        }
    }

    /// Marks the field as one the call must supply.
    #[must_use]
    pub const fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Gives an absent argument this value.
    #[must_use]
    pub const fn with_default(mut self, value: DefaultValue) -> Self {
        self.default = Some(value);
        self
    }

    /// The bounds the retry hint reports for an integer.
    #[must_use]
    pub const fn with_bounds(mut self, min: i64, max: i64) -> Self {
        self.min = Some(min);
        self.max = Some(max);
        self
    }

    /// The sentence the usage block reproduces after the flag.
    #[must_use]
    pub const fn with_description(mut self, description: &'static str) -> Self {
        self.description = Some(description);
        self
    }
}

/// The declared type of a field, spelled as the wire spells it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldType {
    String,
    Integer,
    Boolean,
    Enum,
    Array,
}

impl FieldType {
    /// The wire spelling, which is also what the usage block puts in `<…>`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Boolean => "boolean",
            Self::Enum => "enum",
            Self::Array => "array",
        }
    }
}

/// The item type of an array field. `String` is the absent-`items` default, matching
/// `field.items?.type ?? "string"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Items {
    String,
    Integer,
    Enum(&'static [&'static str]),
}

impl Items {
    /// The closed value set of an enum item type, when it declares one.
    #[must_use]
    pub const fn values(self) -> Option<&'static [&'static str]> {
        match self {
            Self::Enum(values) => Some(values),
            Self::String | Self::Integer => None,
        }
    }
}

/// The value an absent argument takes. Const-friendly where `serde_json::Value` is not, so a
/// manifest can stay a `const` table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DefaultValue {
    Bool(bool),
    Integer(i64),
    Text(&'static str),
    List(&'static [&'static str]),
}

impl DefaultValue {
    /// The value as the argument map stores it.
    #[must_use]
    pub fn json(self) -> Value {
        match self {
            Self::Bool(value) => Value::Bool(value),
            Self::Integer(value) => Value::from(value),
            Self::Text(value) => Value::String(value.to_owned()),
            Self::List(values) => Value::Array(
                values
                    .iter()
                    .map(|item| Value::String((*item).to_owned()))
                    .collect(),
            ),
        }
    }
}

/// One tool's declared arguments, in the order the manifest declares them.
pub type InputLiteral = &'static [(&'static str, InputField)];

/// The token's name part, without `--` and without a `=value` tail.
fn name_of(token: &str) -> &str {
    match token.find('=') {
        Some(eq) => &token[2..eq],
        None => &token[2..],
    }
}

/// The token's `=value` tail, or `None` when it was written as a separate token.
fn inline_value_of(token: &str) -> Option<&str> {
    token.find('=').map(|eq| &token[eq + 1..])
}

fn is_global_value_flag(name: &str) -> bool {
    GLOBAL_VALUE_FLAGS.contains(&name)
}

/// The global flags, removed from argv: the two switches plus everything a tool still has to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Globals {
    /// `--json` appeared anywhere on the line.
    pub json: bool,
    /// `--help` or `-h` appeared anywhere on the line.
    pub help: bool,
    /// Every token that is not a global, in the order the user wrote them.
    pub rest: Vec<String>,
}

/// Removes the global flags from argv and reports the two switches.
///
/// The scan is order-preserving and leaves everything else untouched, so the tokens a tool still
/// has to read arrive in the order the user wrote them. A value flag with nothing to consume is
/// refused here rather than silently treated as a switch, which is the failure a user only notices
/// after the index was built in the wrong place. A token beginning with `--` is never taken as a
/// value; a single dash is, so a negative number stays a value.
pub fn split_globals(argv: &[String]) -> Result<Globals, CliUsageError> {
    let mut json = false;
    let mut help = false;
    let mut rest = Vec::new();

    let mut index = 0;
    while index < argv.len() {
        let token = argv[index].as_str();
        if token == "--json" {
            json = true;
            index += 1;
            continue;
        }
        if token == "--help" || token == "-h" {
            help = true;
            index += 1;
            continue;
        }
        if !token.starts_with("--") || !is_global_value_flag(name_of(token)) {
            rest.push(token.to_owned());
            index += 1;
            continue;
        }
        let name = name_of(token);
        if let Some(inline) = inline_value_of(token) {
            // `--repo-root=` names the flag and withholds the value, which is a typo, not an empty
            // root.
            if inline.is_empty() {
                return Err(CliUsageError::new(
                    format!("--{name} needs a value"),
                    GLOBAL_USAGE,
                ));
            }
            index += 1;
            continue;
        }
        match argv.get(index + 1) {
            Some(next) if !next.starts_with("--") => index += 2,
            _ => {
                return Err(CliUsageError::new(
                    format!("--{name} needs a value"),
                    GLOBAL_USAGE,
                ));
            }
        }
    }

    Ok(Globals { json, help, rest })
}

/// Where the command name sits in argv, and what is left for the command once it is removed.
///
/// The entry point needs this before any config is loaded, because the token that names the verb is
/// also the token that decides whether a verb or a tool answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandResolution {
    /// The first token that is neither a global switch nor a global value flag's value.
    pub name: Option<String>,
    /// Every token after `name`, unchanged, globals retained.
    pub rest: Vec<String>,
    /// `--help` or `-h` anywhere in argv.
    pub help: bool,
}

/// The command name and its tail, found without validating anything: the global flags are known
/// here, so `sangkar --json status` and `sangkar status --json` name the same command. A value flag
/// with nothing to consume is skipped on its own rather than refused — this scan runs before a
/// usage block exists to refuse with, so the verb or the tool reads the dangling token later and
/// says so.
#[must_use]
pub fn resolve_command(argv: &[String]) -> CommandResolution {
    // Read across the whole line, not just the tokens ahead of the name: `sangkar trace --help` must
    // refuse to run anything, and the name is only known after the globals are skipped.
    let help = argv.iter().any(|token| token == "--help" || token == "-h");

    let mut index = 0;
    while index < argv.len() {
        let token = argv[index].as_str();
        if token == "--json" || token == "--help" || token == "-h" {
            index += 1;
            continue;
        }
        if token.starts_with("--") && is_global_value_flag(name_of(token)) {
            // The `=value` form carries its value in the token; the spaced form consumes the next
            // one only when there is one that is not another flag.
            let next = argv.get(index + 1);
            if inline_value_of(token).is_none() && next.is_some_and(|next| !next.starts_with("--"))
            {
                index += 1;
            }
            index += 1;
            continue;
        }
        return CommandResolution {
            name: Some(token.to_owned()),
            rest: argv[index + 1..].to_vec(),
            help,
        };
    }

    CommandResolution {
        name: None,
        rest: Vec::new(),
        help,
    }
}

/// The one field a bare token may bind: the single required string argument of the tool. `trace`,
/// `search` and `expand` each declare exactly one, which is what makes `sangkar search pom.xml` the
/// obvious spelling; a literal whose required field is an enum, or which has two required fields,
/// gets no positional so nothing is guessed.
#[must_use]
pub fn positional_field_of(input: InputLiteral) -> Option<&'static str> {
    let required: Vec<&'static str> = input
        .iter()
        .filter(|(_, field)| field.required && field.kind == FieldType::String)
        .map(|(name, _)| *name)
        .collect();
    match required.as_slice() {
        [only] => Some(only),
        _ => None,
    }
}

fn field_of(input: InputLiteral, name: &str) -> Option<&'static InputField> {
    input
        .iter()
        .find(|(declared, _)| *declared == name)
        .map(|(_, field)| field)
}

/// The value an integer argument carries, as JavaScript's `Number.isInteger` reads it.
///
/// A JSON client may send `2.0` where JavaScript has one number type and `Number.isInteger(2.0)` is
/// true, so a float with no fraction counts and is handed back as the integer it spells: writing it
/// back as `2.0` would be a different byte string from `JSON.stringify`'s.
fn integer_of_number(value: f64) -> Option<i64> {
    // 2^63: `i64::MAX as f64` rounds up to it, so the exclusive bound is the exact one.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if value.is_finite() && value.fract() == 0.0 && (-LIMIT..LIMIT).contains(&value) {
        Some(value as i64)
    } else {
        None
    }
}

/// The integer a JSON number spells, if it spells one.
fn integer_of(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().and_then(integer_of_number)),
        _ => None,
    }
}

/// The number a shell token spells, read with `Number`'s rules.
///
/// `None` stands for `NaN`, which `Number.isInteger` refuses. The empty token is `0` because
/// `Number("")` is: the scalar integer branch of argv reading inherits that, exactly as the
/// TypeScript does. Array items reject a whitespace-only token before arriving here.
fn number_of_text(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Some(0.0);
    }
    // `Number("0x10")` is 16; the three radix prefixes are the one spelling `f64::parse` lacks.
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0b", 2),
        ("0B", 2),
        ("0o", 8),
        ("0O", 8),
    ] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            return i64::from_str_radix(digits, radix)
                .ok()
                .map(|value| value as f64);
        }
    }
    trimmed.parse::<f64>().ok()
}

/// The integer a shell token spells, if it spells one.
fn integer_of_text(text: &str) -> Option<i64> {
    number_of_text(text).and_then(integer_of_number)
}

/// The declared item type of an array field; a manifest that declares none items strings.
fn item_of(field: &InputField, value: &Value) -> Option<Value> {
    match field.items {
        Items::Integer => match value {
            // `itemOf` maps a whitespace-only token to `NaN`, so `--range 1:` is refused rather than
            // read as the zero `Number("")` would give.
            Value::String(text) if text.trim().is_empty() => None,
            Value::String(text) => integer_of_text(text).map(Value::from),
            other => integer_of(other).map(Value::from),
        },
        Items::String | Items::Enum(_) => match value {
            Value::String(text) => match field.items.values() {
                None => Some(value.clone()),
                Some(values) if values.contains(&text.as_str()) => Some(value.clone()),
                Some(_) => None,
            },
            _ => None,
        },
    }
}

fn matches_item(field: &InputField, value: &Value) -> bool {
    item_of(field, value).is_some()
}

/// Every item in the declared type; call only after [`matches_type`] accepted the array.
fn coerce_items(field: &InputField, items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .filter_map(|item| item_of(field, item))
        .collect()
}

/// Whether `value` is the declared type.
#[must_use]
pub fn matches_type(field: &InputField, value: &Value) -> bool {
    match field.kind {
        FieldType::String => matches!(value, Value::String(_)),
        FieldType::Integer => integer_of(value).is_some(),
        FieldType::Boolean => matches!(value, Value::Bool(_)),
        FieldType::Enum => match value {
            Value::String(text) => field.values.contains(&text.as_str()),
            _ => false,
        },
        FieldType::Array => match value {
            Value::Array(items) => items.iter().all(|item| matches_item(field, item)),
            _ => false,
        },
    }
}

/// What the argument should have been, in the one line the agent's retry needs.
#[must_use]
pub fn expectation_of(name: &str, field: &InputField) -> String {
    match field.kind {
        FieldType::Integer => {
            let floor = field.min.unwrap_or(MIN_SAFE_INTEGER);
            let ceiling = field.max.unwrap_or(MAX_SAFE_INTEGER);
            format!("\"{name}\" must be an integer between {floor} and {ceiling}")
        }
        FieldType::Enum => format!("\"{name}\" must be one of: {}", field.values.join(", ")),
        FieldType::Array => match field.items.values() {
            Some(values) => format!("\"{name}\" must be an array of: {}", values.join(", ")),
            None => format!(
                "\"{name}\" must be an array of {}",
                if field.items == Items::Integer {
                    "integers"
                } else {
                    "strings"
                }
            ),
        },
        FieldType::Boolean => format!("\"{name}\" must be true or false"),
        FieldType::String => format!("\"{name}\" must be a string"),
    }
}

/// Arguments against the manifest literal: unknown keys are dropped, declared defaults fill an
/// absent argument, a missing required argument and a wrong type are refused. Refusals carry the
/// closed `internal` code because they are our own schema disagreeing with the client, and the hint
/// names the argument so the retry is a correction rather than a guess.
pub fn parse_args(tool: &str, input: InputLiteral, raw: &Value) -> Result<ToolArgs, AppError> {
    let source = raw.as_object();
    let mut args = ToolArgs::new();
    for (name, field) in input {
        let value = source.and_then(|source| source.get(*name));
        let Some(value) = value else {
            if let Some(default) = field.default {
                args.insert((*name).to_owned(), default.json());
            } else if field.required {
                return Err(AppError::internal(format!(
                    "{tool} is missing the required argument \"{name}\""
                ))
                .with_hint(expectation_of(name, field)));
            }
            continue;
        };
        if !matches_type(field, value) {
            return Err(AppError::internal(format!(
                "{tool} received an invalid \"{name}\" argument"
            ))
            .with_hint(expectation_of(name, field)));
        }
        // A validated array is handed over in its declared item type: `matches_type` accepted the
        // text form of an integer item, and the handler should read numbers rather than re-parse
        // strings. A validated integer is handed over as an integer for the same reason.
        let stored = match field.kind {
            FieldType::Integer => integer_of(value).map_or_else(|| value.clone(), Value::from),
            FieldType::Array => match value {
                Value::Array(items) => Value::Array(coerce_items(field, items)),
                other => other.clone(),
            },
            _ => value.clone(),
        };
        args.insert((*name).to_owned(), stored);
    }
    Ok(args)
}

/// argv read into the record [`parse_args`] validates: the declared names the caller wrote and
/// nothing else, since an unknown option is refused rather than kept. It is therefore also the
/// answer to "which arguments were supplied" — a default is added afterwards, by [`parse_args`],
/// and a defaulted name is not a fact about the request.
///
/// Only the parts a shell cannot express are lost: an array field collects repeats (`--paths a
/// --paths b`) and never splits a comma, because a path may contain one. An array of integers is
/// the one spelling a shell would never write as a repeat — `--range 1:5` is the pair — so that
/// item type splits on the colon and leaves everything else whole. `min`/`max` are left to the
/// handler, exactly as a protocol call leaves them. Every shell-level refusal is a
/// [`CliUsageError`] so the caller prints usage instead of a protocol error.
fn read_argv(tool: &str, input: InputLiteral, argv: &[String]) -> Result<ToolArgs, CliUsageError> {
    let usage = usage_of(tool, input);
    let positional = positional_field_of(input);
    let mut raw = ToolArgs::new();

    let mut index = 0;
    while index < argv.len() {
        let token = argv[index].as_str();
        if !token.starts_with("--") {
            match positional {
                Some(name) if !raw.contains_key(name) => {
                    raw.insert(name.to_owned(), Value::String(token.to_owned()));
                }
                _ => {
                    return Err(CliUsageError::new(
                        format!("unexpected positional argument \"{token}\""),
                        usage,
                    ));
                }
            }
            index += 1;
            continue;
        }

        let written = name_of(token);
        let inline = inline_value_of(token);
        // `--no-force` is the switch spelling of `--force=false`; a field literally named `no-…`
        // wins.
        let negated = match inline {
            None => written.strip_prefix("no-"),
            Some(_) => None,
        };
        let declared = field_of(input, written);
        let name = if declared.is_none() {
            negated.unwrap_or(written)
        } else {
            written
        };
        let Some(field) = field_of(input, name) else {
            return Err(CliUsageError::new(
                format!("unknown option \"--{written}\""),
                usage,
            ));
        };

        if name != written {
            if field.kind != FieldType::Boolean {
                return Err(CliUsageError::new(
                    format!("unknown option \"--{written}\""),
                    usage,
                ));
            }
            raw.insert(name.to_owned(), Value::Bool(false));
            index += 1;
            continue;
        }

        if field.kind == FieldType::Boolean {
            let value = match inline {
                None => true,
                Some("true") => true,
                Some("false") => false,
                Some(_) => {
                    return Err(CliUsageError::new(
                        format!("--{name} must be true or false"),
                        usage,
                    ));
                }
            };
            raw.insert(name.to_owned(), Value::Bool(value));
            index += 1;
            continue;
        }

        let (value, spaced) = match inline {
            // `--depth=` withholds the value rather than passing the empty string: no argument here
            // is one.
            Some("") => {
                return Err(CliUsageError::new(format!("--{name} needs a value"), usage));
            }
            Some(text) => (text, false),
            None => match argv.get(index + 1) {
                Some(next) if !next.starts_with("--") => (next.as_str(), true),
                _ => {
                    return Err(CliUsageError::new(format!("--{name} needs a value"), usage));
                }
            },
        };

        match field.kind {
            FieldType::Integer => {
                let Some(number) = integer_of_text(value) else {
                    return Err(CliUsageError::new(
                        format!("--{name} must be an integer"),
                        usage,
                    ));
                };
                raw.insert(name.to_owned(), Value::from(number));
            }
            FieldType::Array => {
                // The item type decides the token: a pair written as one value is the shell's form
                // of a `[from, to]` array of integers, while a string array keeps each token whole.
                let parts: Vec<&str> = if field.items == Items::Integer {
                    value.split(':').collect()
                } else {
                    vec![value]
                };
                let collected = raw
                    .entry(name.to_owned())
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(items) = collected {
                    items.extend(parts.into_iter().map(|part| Value::String(part.to_owned())));
                }
            }
            _ => {
                raw.insert(name.to_owned(), Value::String(value.to_owned()));
            }
        }
        index += if spaced { 2 } else { 1 };
    }

    Ok(raw)
}

/// One CLI call as the argument layer sees it: the record a handler receives, and the names the
/// caller actually wrote. The two differ on purpose — `args` carries every declared default, so
/// recording its keys would make the supplied set a constant of the schema.
#[derive(Clone, Debug, PartialEq)]
pub struct ParsedCliArgs {
    pub args: ToolArgs,
    /// The declared names the caller wrote, sorted, before any default.
    pub supplied: Vec<String>,
}

/// argv for one tool, without the tool token and without the global flags, read against the same
/// manifest and handed to [`parse_args`]. Every shell-level refusal is a [`CliUsageError`] so the
/// caller prints usage instead of a protocol error; the two schema refusals keep the message
/// [`parse_args`] produced, so a CLI user reads what an agent would read.
pub fn parse_cli_args(
    tool: &str,
    input: InputLiteral,
    argv: &[String],
) -> Result<ToolArgs, CliUsageError> {
    parse_cli_args_and_names(tool, input, argv).map(|parsed| parsed.args)
}

/// [`parse_cli_args`] plus the supplied names: `read_argv` only ever holds declared names, so its
/// keys are exactly the arguments whose source value was absent, before any default.
pub fn parse_cli_args_and_names(
    tool: &str,
    input: InputLiteral,
    argv: &[String],
) -> Result<ParsedCliArgs, CliUsageError> {
    let raw = read_argv(tool, input, argv)?;
    let mut supplied: Vec<String> = raw.keys().cloned().collect();
    supplied.sort();
    match parse_args(tool, input, &Value::Object(raw)) {
        Ok(args) => Ok(ParsedCliArgs { args, supplied }),
        Err(error) => Err(CliUsageError::new(error.message, usage_of(tool, input))),
    }
}

/// The usage block for one tool, generated from its manifest so a new argument cannot ship
/// undocumented. The header names the positional a bare token binds; the last line names the flags
/// that belong to no tool and are therefore absent from every manifest's field list.
#[must_use]
pub fn usage_of(tool: &str, input: InputLiteral) -> String {
    let header = match positional_field_of(input) {
        Some(positional) => format!("sangkar {tool} [<{positional}>] [options]"),
        None => format!("sangkar {tool} [options]"),
    };
    let mut lines = vec![header];
    for (name, field) in input {
        let marks = if field.required { " (required)" } else { "" };
        let description = field
            .description
            .map(|description| format!("  {description}"))
            .unwrap_or_default();
        lines.push(format!(
            "  --{name} <{}>{marks}{description}",
            field.kind.name()
        ));
    }
    lines.push(
        "global options: --repo-root <path>, --data-dir <path>, --json (JSON instead of TOON)"
            .to_owned(),
    );
    lines.join("\n")
}

/// The verbs this process owns; every other first token is a tool name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Verb {
    Mcp,
    Worker,
    Index,
    Telemetry,
    Daemon,
    Repo,
}

impl Verb {
    /// The six, in the order the usage guide lists them.
    pub const ALL: [Self; 6] = [
        Self::Mcp,
        Self::Worker,
        Self::Index,
        Self::Telemetry,
        Self::Daemon,
        Self::Repo,
    ];

    /// The spelling on the command line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Worker => "worker",
            Self::Index => "index",
            Self::Telemetry => "telemetry",
            Self::Daemon => "daemon",
            Self::Repo => "repo",
        }
    }

    /// The verb a command name denotes, if it denotes one.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|verb| verb.name() == name)
    }
}

/// How a verb's own flag is spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagType {
    /// Bare presence is true; `=true`, `=false` and `--no-<name>` are the other spellings, and the
    /// next token is never consumed as a value.
    Switch,
    /// A value is required and must be `true` or `false`: a shell has no boolean type, so `1`, `yes`
    /// and `"false"` would each be a guess — and guessing wrong here writes a file that says the
    /// opposite.
    Bool,
    /// A value is required.
    Text,
    /// A value is required and must be a whole number.
    Integer,
    /// A value is required and must be one of the listed spellings.
    Enum(&'static [&'static str]),
}

/// One flag a verb accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerbFlag {
    /// The spelling without `--`.
    pub name: &'static str,
    pub kind: FlagType,
    /// The subcommands that accept it. Empty means the verb itself, which is only the case for a
    /// verb that takes no subcommand: a flag a subcommand verb accepts for every subcommand would
    /// list them all.
    pub subcommands: &'static [&'static str],
}

/// One verb's flag set and usage block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VerbSpec {
    pub verb: Verb,
    /// The verb's own flags. The four globals are not here: [`split_globals`] removes them before
    /// this table is consulted, which is what makes them acceptable to every verb.
    pub flags: &'static [VerbFlag],
    /// The subcommands this verb accepts, in the order its usage block lists them; empty means the
    /// verb takes no subcommand.
    pub subcommands: &'static [&'static str],
    /// The block a refusal prints and `--help` answers, verbatim from the TypeScript side.
    pub usage: &'static str,
}

/// The verb flag table: every flag the TypeScript CLI accepts today, as data.
pub const VERB_SPECS: [VerbSpec; 6] = [
    VerbSpec {
        verb: Verb::Mcp,
        flags: &[],
        subcommands: &[],
        usage: "usage: sangkar mcp [--repo-root <path>] [--data-dir <path>] [--json]",
    },
    VerbSpec {
        verb: Verb::Worker,
        flags: &[
            VerbFlag {
                name: "mode",
                kind: FlagType::Enum(&["full", "fast"]),
                subcommands: &[],
            },
            VerbFlag {
                name: "force",
                kind: FlagType::Switch,
                subcommands: &[],
            },
        ],
        subcommands: &[],
        usage: "usage: sangkar worker [--mode full|fast] [--force] [--repo-root <path>] [--data-dir <path>]",
    },
    VerbSpec {
        verb: Verb::Index,
        flags: &[
            VerbFlag {
                name: "mode",
                kind: FlagType::Enum(&["full", "fast"]),
                subcommands: &[],
            },
            VerbFlag {
                name: "force",
                kind: FlagType::Switch,
                subcommands: &[],
            },
            // `--yes` belongs to the verb, not to the worker: it answers the question the verb asks
            // before it spawns one, when the tree was autodetected above the working directory and
            // stdin is a terminal.
            VerbFlag {
                name: "yes",
                kind: FlagType::Switch,
                subcommands: &[],
            },
            // The opt-out is its own spelling rather than `--register=false`: that is what the
            // TypeScript reads (`argv.includes("--no-register")`), and accepting `--register` as
            // well would advertise a flag the reference CLI does not have.
            VerbFlag {
                name: "no-register",
                kind: FlagType::Switch,
                subcommands: &[],
            },
        ],
        subcommands: &[],
        usage: "usage: sangkar index [--mode full|fast] [--force] [--repo-root <path>] [--data-dir <path>] [--yes]\n  --yes          index the autodetected tree without asking (the answer a script gives)",
    },
    VerbSpec {
        verb: Verb::Telemetry,
        flags: &[
            VerbFlag {
                name: "out",
                kind: FlagType::Text,
                subcommands: &[],
            },
            VerbFlag {
                name: "serve",
                kind: FlagType::Switch,
                subcommands: &[],
            },
            VerbFlag {
                name: "port",
                kind: FlagType::Integer,
                subcommands: &[],
            },
            VerbFlag {
                name: "host",
                kind: FlagType::Text,
                subcommands: &[],
            },
            VerbFlag {
                name: "since",
                kind: FlagType::Text,
                subcommands: &[],
            },
        ],
        subcommands: &[],
        usage: "usage: sangkar telemetry [--out <path>] [--serve] [--port <n>] [--host <addr>] [--since <YYYY-MM-DD|Nd>] [--repo-root <path>] [--data-dir <path>]\n  --serve        serve the report for every registered repository on loopback instead of writing one file\n  --port <n>     the port to bind (default: config.json's daemon.port, else 7317; 0 picks a free port)\n  --host <addr>  the address to bind (default: config.json's daemon.host, else 127.0.0.1; loopback only)",
    },
    VerbSpec {
        verb: Verb::Daemon,
        flags: &[
            VerbFlag {
                name: "port",
                kind: FlagType::Integer,
                subcommands: &["run"],
            },
            VerbFlag {
                name: "host",
                kind: FlagType::Text,
                subcommands: &["run"],
            },
        ],
        subcommands: &["run", "start", "stop", "status"],
        usage: "usage: sangkar daemon [run|start|stop|status] [--port <n>] [--host <addr>]\n  run     run the daemon in the foreground until a signal stops it\n  start   start it as a detached child and return once it holds the pid file\n  stop    stop the daemon this home's pid file names\n  status  one header line, then one line per registered repository\n  --port <n>     the report server's port (default: config.json's daemon.port, else 7317; 0 picks a free port)\n  --host <addr>  the address to bind (default: config.json's daemon.host, else 127.0.0.1; loopback only)\n  --repo-root and --data-dir are accepted by every subcommand and change nothing here; --json prints status as one JSON object line, and --port/--host still belong to \"run\"\n  --port and --host belong to \"run\"; start passes neither, so it binds the same defaults",
    },
    VerbSpec {
        verb: Verb::Repo,
        flags: &[
            VerbFlag {
                name: "watch",
                kind: FlagType::Bool,
                subcommands: &["config"],
            },
            VerbFlag {
                name: "watch-interval",
                kind: FlagType::Integer,
                subcommands: &["config"],
            },
            VerbFlag {
                name: "telemetry",
                kind: FlagType::Bool,
                subcommands: &["config"],
            },
        ],
        subcommands: &["add", "remove", "list", "config"],
        usage: "usage: sangkar repo [add|remove|list|config] <path>\n  add <path>                register a tree and print its canonical path\n  remove <path>             unregister a tree; the tree and its graph are left alone\n  list                      one registered path per line (--json for a JSON document)\n  config <path>             write <path>/.sangkar/config.json and print the effective settings\n  --json                    the registry as {\"version\":1,\"repos\":[...]} (list only)\n  --watch <true|false>      whether the daemon watches this tree (config only)\n  --watch-interval <n>      poll period in milliseconds, 1000..300000 (config only)\n  --telemetry <true|false>  whether this tree's usage may be recorded (config only)\n  --repo-root, --data-dir and --json are accepted by every subcommand; only list changes with --json\n  the <path> may follow the flags, and no token starting with -- is ever read as a path\n  a relative <path> resolves against the current directory; a leading ~/ is expanded",
    },
];

/// The table row for one verb.
#[must_use]
pub fn verb_spec(verb: Verb) -> &'static VerbSpec {
    VERB_SPECS
        .iter()
        .find(|spec| spec.verb == verb)
        .expect("every verb has a table row")
}

/// One flag's value, in the type its table row declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgValue {
    Bool(bool),
    Integer(i64),
    Text(String),
}

/// One verb invocation, read against its table row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerbArgs {
    pub verb: Verb,
    /// The subcommand the caller named, or `None` for a verb that takes none.
    pub subcommand: Option<&'static str>,
    /// The bare tokens, in the order written: `repo`'s `<path>` is the first of them.
    pub positionals: Vec<String>,
    flags: BTreeMap<&'static str, ArgValue>,
}

impl VerbArgs {
    /// An invocation with no flag supplied yet.
    #[must_use]
    pub fn new(verb: Verb, subcommand: Option<&'static str>) -> Self {
        Self {
            verb,
            subcommand,
            positionals: Vec::new(),
            flags: BTreeMap::new(),
        }
    }

    /// One flag's value.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&ArgValue> {
        self.flags.get(name)
    }

    /// Whether a switch was given as true.
    #[must_use]
    pub fn switch(&self, name: &str) -> bool {
        matches!(self.flags.get(name), Some(ArgValue::Bool(true)))
    }

    /// A boolean flag's value.
    #[must_use]
    pub fn boolean(&self, name: &str) -> Option<bool> {
        match self.flags.get(name) {
            Some(ArgValue::Bool(value)) => Some(*value),
            _ => None,
        }
    }

    /// An integer flag's value.
    #[must_use]
    pub fn integer(&self, name: &str) -> Option<i64> {
        match self.flags.get(name) {
            Some(ArgValue::Integer(value)) => Some(*value),
            _ => None,
        }
    }

    /// A text flag's value.
    #[must_use]
    pub fn text(&self, name: &str) -> Option<&str> {
        match self.flags.get(name) {
            Some(ArgValue::Text(value)) => Some(value),
            _ => None,
        }
    }

    /// The first positional, which is the one `repo` reads as its `<path>`.
    #[must_use]
    pub fn positional(&self) -> Option<&str> {
        self.positionals.first().map(String::as_str)
    }
}

/// One verb call: the globals it carried and the flags it was read into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerbInvocation {
    pub globals: Globals,
    pub args: VerbArgs,
}

/// The subcommand the first token names, and the tokens the flag reader still has to read.
fn split_subcommand<'a>(
    spec: &VerbSpec,
    rest: &'a [String],
) -> Result<(Option<&'static str>, &'a [String]), CliUsageError> {
    if spec.subcommands.is_empty() {
        return Ok((None, rest));
    }
    let usage = spec.usage;
    let Some(first) = rest.first() else {
        return Err(CliUsageError::new(
            format!("{} needs a subcommand", spec.verb.name()),
            usage,
        ));
    };
    match spec
        .subcommands
        .iter()
        .copied()
        .find(|name| *name == first.as_str())
    {
        Some(name) => Ok((Some(name), &rest[1..])),
        // A flag written ahead of the subcommand lands here too, and the message names the token
        // the caller actually typed, exactly as the TypeScript verbs do.
        None => Err(CliUsageError::new(
            format!("unknown subcommand \"{first}\""),
            usage,
        )),
    }
}

/// The table row for one written flag, refusing a flag this verb or subcommand does not take.
fn lookup_flag<'a>(
    spec: &'a VerbSpec,
    token: &str,
    written: &str,
    subcommand: Option<&str>,
    usage: &str,
) -> Result<&'a VerbFlag, CliUsageError> {
    let Some(flag) = spec.flags.iter().find(|flag| flag.name == written) else {
        return Err(CliUsageError::new(
            format!("unknown option \"--{written}\""),
            usage,
        ));
    };
    let applies = flag.subcommands.is_empty()
        || subcommand.is_some_and(|chosen| flag.subcommands.contains(&chosen));
    if !applies {
        let scope = match subcommand {
            Some(chosen) => format!("\"{} {chosen}\"", spec.verb.name()),
            None => format!("\"{}\"", spec.verb.name()),
        };
        return Err(CliUsageError::new(
            format!("{scope} takes no {token}"),
            usage,
        ));
    }
    Ok(flag)
}

/// One verb's own tokens, after [`split_globals`], read against its table row.
///
/// Every refusal carries the verb's usage block. A flag outside the row, and a row flag the chosen
/// subcommand does not take, are both refused: the TypeScript checks the second one per subcommand
/// (`repo.ts`, `daemon/cli.ts`), and this port makes the first one a rule for every verb instead of
/// leaving an unrecognised flag to be silently ignored.
pub fn parse_verb_args(verb: Verb, rest: &[String]) -> Result<VerbArgs, CliUsageError> {
    let spec = verb_spec(verb);
    let usage = spec.usage;
    let (subcommand, tokens) = split_subcommand(spec, rest)?;
    let mut args = VerbArgs::new(verb, subcommand);

    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index].as_str();
        if !token.starts_with("--") {
            // A token beginning with `--` is never a path: accepting one is how the settings for
            // `--watch` were written into a directory named `--watch` while the answer looked right.
            args.positionals.push(token.to_owned());
            index += 1;
            continue;
        }

        let written = name_of(token);
        let inline = inline_value_of(token);
        let negated = match inline {
            None => written.strip_prefix("no-"),
            Some(_) => None,
        };
        let flag = if spec.flags.iter().any(|flag| flag.name == written) {
            lookup_flag(spec, token, written, subcommand, usage)?
        } else {
            // `--no-force` is the switch spelling of `--force=false`, and only a switch has one: a
            // field literally named `no-…` wins, which is why the direct lookup runs first.
            match negated {
                Some(base)
                    if spec
                        .flags
                        .iter()
                        .any(|flag| flag.name == base && flag.kind == FlagType::Switch) =>
                {
                    lookup_flag(spec, token, base, subcommand, usage)?
                }
                _ => {
                    return Err(CliUsageError::new(
                        format!("unknown option \"--{written}\""),
                        usage,
                    ));
                }
            }
        };
        let negated = flag.name != written;

        match flag.kind {
            FlagType::Switch => {
                let value = match inline {
                    None => true,
                    Some("true") => true,
                    Some("false") => false,
                    Some(_) => {
                        return Err(CliUsageError::new(
                            format!("--{} must be true or false", flag.name),
                            usage,
                        ));
                    }
                };
                // The first spelling wins, as `flagValue` answers the first occurrence: a later
                // repeat cannot flip a decision the caller already made.
                args.flags
                    .entry(flag.name)
                    .or_insert(ArgValue::Bool(if negated { false } else { value }));
                index += 1;
                continue;
            }
            FlagType::Bool => {
                let value = match inline {
                    Some(text) => Some(text),
                    None => match tokens.get(index + 1) {
                        Some(next) if !next.starts_with("--") => Some(next.as_str()),
                        _ => None,
                    },
                };
                let Some(text) = value else {
                    return Err(CliUsageError::new(
                        format!("--{} needs true or false", flag.name),
                        usage,
                    ));
                };
                let parsed = match text {
                    "true" => true,
                    "false" => false,
                    other => {
                        return Err(CliUsageError::new(
                            format!("--{} must be true or false, not \"{other}\"", flag.name),
                            usage,
                        ));
                    }
                };
                args.flags
                    .entry(flag.name)
                    .or_insert(ArgValue::Bool(parsed));
                index += if inline.is_some() { 1 } else { 2 };
                continue;
            }
            _ => {}
        }

        let (value, spaced) = match inline {
            Some("") => {
                return Err(CliUsageError::new(
                    format!("--{} needs a value", flag.name),
                    usage,
                ));
            }
            Some(text) => (text, false),
            None => match tokens.get(index + 1) {
                Some(next) if !next.starts_with("--") => (next.as_str(), true),
                _ => {
                    return Err(CliUsageError::new(
                        format!("--{} needs a value", flag.name),
                        usage,
                    ));
                }
            },
        };

        let stored = match flag.kind {
            FlagType::Integer => {
                let Some(number) = integer_of_text(value) else {
                    return Err(CliUsageError::new(
                        format!("--{} must be an integer", flag.name),
                        usage,
                    ));
                };
                ArgValue::Integer(number)
            }
            FlagType::Enum(values) => {
                if !values.contains(&value) {
                    return Err(CliUsageError::new(
                        format!("--{} must be one of: {}", flag.name, values.join(", ")),
                        usage,
                    ));
                }
                ArgValue::Text(value.to_owned())
            }
            _ => ArgValue::Text(value.to_owned()),
        };
        args.flags.entry(flag.name).or_insert(stored);
        index += if spaced { 2 } else { 1 };
    }

    Ok(args)
}

/// One verb line, globals included: [`split_globals`] first, then [`parse_verb_args`]. A
/// [`split_globals`] refusal is re-pointed at the verb's own block, which is what the TypeScript
/// verbs do before dispatching on their subcommand.
///
/// `--help` is answered by the caller *before* this call, exactly as every TypeScript verb answers
/// it before validating anything: a caller asking for the shape of the command must not have to
/// satisfy it first. [`split_globals`] is where the switch is read.
pub fn parse_verb(verb: Verb, argv: &[String]) -> Result<VerbInvocation, CliUsageError> {
    let usage = verb_spec(verb).usage;
    let globals = split_globals(argv).map_err(|error| CliUsageError::new(error.message, usage))?;
    let args = parse_verb_args(verb, &globals.rest)?;
    Ok(VerbInvocation { globals, args })
}
