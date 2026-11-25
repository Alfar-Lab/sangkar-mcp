//! The seven tools this build publishes, in the order `tools/list` reports them: the one table the
//! MCP surface, the CLI and the protocol layer all read.
//!
//! Ported from the registry half of `src/index.ts` (`TOOLS`, `propertySchemaOf`, `itemSchemaOf`,
//! `inputSchemaOf`, `descriptorOf`). Registration is an explicit list — seven lines, no directory
//! scan — so adding a tool is a visible edit and the dependency graph stays readable.
//!
//! # Why this file may name all seven tool modules
//!
//! `sangkar-boundary`'s tool-isolation rule forbids one **tool module** naming another, and its rule
//! is anchored at `src/<tool>/**`. This file is the crate root, not a tool: reconciling seven
//! manifests is exactly the job that needs to see all seven, which is why the rule was written
//! against the tool directories rather than the whole crate.
//!
//! Each manifest declares its **own** `Param`/`ParamType`/`ParamDefault`/`ExampleValue`, because a
//! tool module may not import a sibling's vocabulary. They are convertible but not interchangeable:
//! `status` has no bounds and no enum, `search` and `summary` have no boolean, `expand` is the only
//! one with an integer array, `index` the only one with a boolean. So this file does not move the
//! types and does not edit the seven manifests: it converts each into the one [`RegisteredParam`]
//! shape, one explicit block per tool, in the documented order. Deleting the local vocabularies is a
//! later refactor with its own task; a conversion that is boring and mechanical is what keeps the
//! published schema provably the manifest's own data.
//!
//! # What the descriptor is
//!
//! A `tools/list` entry is `{name, description, inputSchema}`, and `inputSchema` is
//! `{type: "object", properties, required?, examples?}` — `required` only when something is
//! required, `examples` only when the manifest declares one call. Neither is a preference: both were
//! captured from the TypeScript server (`rust/tests/vectors/mcp_session.ndjson`, pinned by
//! `sangkar-cli/tests/dispatch.rs`), so an omitted or differently ordered key is a parity failure.
//!
//! The conversion is the one `src/index.ts:227-275` performs, and the three places it is not a
//! spelling change:
//!
//! - JSON Schema has **no enum type**, so a declared enum is `{type: "string", enum: [...]}` and an
//!   array of enums is `{type: "array", items: {type: "string", enum: [...]}}`;
//! - `min`/`max` become `minimum`/`maximum`, which only an integer declares;
//! - a declared default travels **with the property**, so a client can see what an omitted argument
//!   means before sending the call.
//!
//! # The handler seam
//!
//! Every handler takes its own typed arguments (`&TraceArgs`, `&IndexArgs`, …), and the CLI hands
//! over the `Map` the argument parser produced. [`Handler`] is therefore one uniform signature over
//! that map, and each tool's block carries the small conversion from the map to its typed struct —
//! declared keys only, so a handler never re-derives a default the manifest already states. A value
//! outside a declared enum cannot reach the conversion (`parse_args` refuses it against this same
//! table), and every enum field in the seven arg structs is an `Option` whose `None` *is* the
//! declared default, so an absent value needs no branch.

use sangkar_core::envelope::Envelope;
use sangkar_store::read::context::ToolContext;
use serde::Serialize;
use serde_json::{Map, Value};

// The seven tool modules, each named once. The crate root is the one place the isolation rule
// allows this: a file under `src/<tool>/` may not name another, and this file is not a tool.
use crate::expand::manifest::ExampleValue as ExpandExample;
use crate::impact::manifest::ExampleValue as ImpactExample;
use crate::index::manifest::ExampleValue as IndexExample;
use crate::search::manifest::ExampleValue as SearchExample;
use crate::status::manifest::ExampleValue as StatusExample;
use crate::summary::manifest::ExampleValue as SummaryExample;
use crate::trace::manifest::ExampleValue as TraceExample;
use crate::{expand, impact, index, search, status, summary, trace};

/// The argument map the parser produced: declared keys only, defaults applied, in declaration order.
pub type Args = Map<String, Value>;

/// One tool's answer. The uniform signature is what lets the table hold seven different handlers:
/// the handler returns an envelope, never an error, and never writes to a stream.
pub type Handler = fn(&ToolContext, &Args) -> Envelope<Value>;

/// A declared parameter's type, in the union the seven manifests need.
///
/// One vocabulary rather than seven: this is the shape the descriptor derivation consumes, and the
/// CLI converts it to its own parser field. `Array` carries its item type because JSON Schema has no
/// "array of enum" of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Str,
    Integer,
    Bool,
    /// A closed set of strings.
    Enum(&'static [&'static str]),
    /// A list whose every item is one of `Items`.
    Array(Items),
}

/// The item type of an array parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Items {
    /// Any string, which is what a manifest declares with an empty value set.
    Str,
    Integer,
    /// A closed set of strings.
    Enum(&'static [&'static str]),
}

/// The value an absent argument takes.
///
/// Four variants because four shapes are all the seven manifests declare: a word, a number, a
/// switch, and a list (an absent `paths` is the empty list, not the repository root).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Default {
    Text(&'static str),
    Integer(i64),
    Bool(bool),
    List(&'static [&'static str]),
}

/// One declared example value, in the three scalar shapes the seven manifests use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Example {
    Text(&'static str),
    Integer(i64),
    List(&'static [&'static str]),
}

/// One declared argument, in the one vocabulary the descriptor is rendered from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegisteredParam {
    pub name: &'static str,
    pub kind: Kind,
    pub required: bool,
    pub default: Option<Default>,
    /// Rendered as `minimum`; only an integer declares one.
    pub min: Option<i64>,
    /// Rendered as `maximum`.
    pub max: Option<i64>,
    pub description: &'static str,
}

/// A tool's declared surface, derived from its own manifest on demand.
///
/// Owned rather than `&'static` because it is a *conversion*: seven local vocabularies cannot be
/// concatenated into one static table without editing them, and an explicit per-tool block is what
/// the reconciliation is. Deriving it per call costs one `Vec` allocation per `tools/list`, which is
/// nothing next to the JSON it becomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    /// The declared arguments, in the order `tools/list` publishes the properties.
    pub params: Vec<RegisteredParam>,
    /// The one call the manifest shows, when it declares one: a `(key, value)` list, so this type
    /// needs no JSON parser to be built.
    pub example: Option<Vec<(&'static str, Example)>>,
}

/// One registered tool: its name, the description a model routes on, its descriptor, and its handler.
///
/// A `fn` pointer rather than a pre-built value for the descriptor for the same reason it is owned:
/// `TOOLS` must stay a static table while each descriptor is a conversion of a different manifest.
pub struct RegisteredTool {
    pub name: &'static str,
    pub description: &'static str,
    /// Derives this tool's declared surface from its own manifest.
    pub describe: fn() -> Descriptor,
    pub handle: Handler,
}

impl RegisteredTool {
    /// This tool's `tools/list` entry: `{name, description, inputSchema}`.
    ///
    /// Key order is part of the contract — a client hashing the entry, and the recorded frame this
    /// is pinned against, both see it — so the object is built in the TypeScript's order: `type`,
    /// `properties`, then `required` when there is one, then `examples` when the manifest showed one.
    #[must_use]
    pub fn descriptor(&self) -> Value {
        let Descriptor { params, example } = (self.describe)();
        let mut properties = Map::new();
        let mut required: Vec<Value> = Vec::new();
        for param in &params {
            properties.insert(param.name.to_owned(), property_schema(param));
            if param.required {
                required.push(Value::String(param.name.to_owned()));
            }
        }
        let mut schema = Map::new();
        schema.insert("type".to_owned(), Value::String("object".to_owned()));
        schema.insert("properties".to_owned(), Value::Object(properties));
        if !required.is_empty() {
            schema.insert("required".to_owned(), Value::Array(required));
        }
        if let Some(example) = example {
            schema.insert(
                "examples".to_owned(),
                Value::Array(vec![example_value(&example)]),
            );
        }
        let mut entry = Map::new();
        entry.insert("name".to_owned(), Value::String(self.name.to_owned()));
        entry.insert(
            "description".to_owned(),
            Value::String(self.description.to_owned()),
        );
        entry.insert("inputSchema".to_owned(), Value::Object(schema));
        Value::Object(entry)
    }
}

/// One declared argument as JSON Schema, in the TypeScript's key order: the type, then the value set
/// or the item schema, then the bounds, then the description, then the default.
fn property_schema(param: &RegisteredParam) -> Value {
    let mut schema = Map::new();
    match param.kind {
        Kind::Str => {
            schema.insert("type".to_owned(), Value::String("string".to_owned()));
        }
        Kind::Integer => {
            schema.insert("type".to_owned(), Value::String("integer".to_owned()));
        }
        Kind::Bool => {
            schema.insert("type".to_owned(), Value::String("boolean".to_owned()));
        }
        // JSON Schema has no enum type: the value set is a keyword on a string.
        Kind::Enum(values) => {
            schema.insert("type".to_owned(), Value::String("string".to_owned()));
            schema.insert("enum".to_owned(), strings(values));
        }
        Kind::Array(items) => {
            schema.insert("type".to_owned(), Value::String("array".to_owned()));
            let mut item = Map::new();
            match items {
                Items::Str => {
                    item.insert("type".to_owned(), Value::String("string".to_owned()));
                }
                Items::Integer => {
                    item.insert("type".to_owned(), Value::String("integer".to_owned()));
                }
                Items::Enum(values) => {
                    item.insert("type".to_owned(), Value::String("string".to_owned()));
                    item.insert("enum".to_owned(), strings(values));
                }
            }
            schema.insert("items".to_owned(), Value::Object(item));
        }
    }
    if let Some(min) = param.min {
        schema.insert("minimum".to_owned(), Value::from(min));
    }
    if let Some(max) = param.max {
        schema.insert("maximum".to_owned(), Value::from(max));
    }
    schema.insert(
        "description".to_owned(),
        Value::String(param.description.to_owned()),
    );
    if let Some(default) = param.default {
        schema.insert("default".to_owned(), default_value(default));
    }
    Value::Object(schema)
}

/// A JSON array of the declared strings.
fn strings(values: &[&'static str]) -> Value {
    Value::Array(
        values
            .iter()
            .map(|value| Value::String((*value).to_owned()))
            .collect(),
    )
}

/// A declared default as the JSON the schema carries.
fn default_value(default: Default) -> Value {
    match default {
        Default::Text(text) => Value::String(text.to_owned()),
        Default::Integer(number) => Value::from(number),
        Default::Bool(flag) => Value::Bool(flag),
        Default::List(values) => strings(values),
    }
}

/// The manifest's example call as one JSON object, its pairs in declaration order.
fn example_value(pairs: &[(&'static str, Example)]) -> Value {
    let mut object = Map::new();
    for (name, value) in pairs {
        let value = match value {
            Example::Text(text) => Value::String((*text).to_owned()),
            Example::Integer(number) => Value::from(*number),
            Example::List(values) => strings(values),
        };
        object.insert((*name).to_owned(), value);
    }
    Value::Object(object)
}

/// An array parameter's item type: an empty value set is the plain string array the TypeScript
/// spells `items: { type: "string" }`.
fn items_of(values: &'static [&'static str]) -> Items {
    if values.is_empty() {
        Items::Str
    } else {
        Items::Enum(values)
    }
}

/// The table, in the order `tools/list` reports: the orientation tools first, then the lookups, then
/// the admin repair. All seven are registered unconditionally — one surface for the MCP client and
/// the CLI alike; what keeps a repair out of the way is the instructions, not a filter.
pub static TOOLS: [RegisteredTool; 7] = [
    RegisteredTool {
        name: status::manifest::NAME,
        description: status::manifest::DESCRIPTION,
        describe: status_descriptor,
        handle: status_answer,
    },
    RegisteredTool {
        name: summary::manifest::NAME,
        description: summary::manifest::DESCRIPTION,
        describe: summary_descriptor,
        handle: summary_answer,
    },
    RegisteredTool {
        name: trace::manifest::NAME,
        description: trace::manifest::DESCRIPTION,
        describe: trace_descriptor,
        handle: trace_answer,
    },
    RegisteredTool {
        name: search::manifest::NAME,
        description: search::manifest::DESCRIPTION,
        describe: search_descriptor,
        handle: search_answer,
    },
    RegisteredTool {
        name: impact::manifest::NAME,
        description: impact::manifest::DESCRIPTION,
        describe: impact_descriptor,
        handle: impact_answer,
    },
    RegisteredTool {
        name: expand::manifest::NAME,
        description: expand::manifest::DESCRIPTION,
        describe: expand_descriptor,
        handle: expand_answer,
    },
    RegisteredTool {
        name: index::manifest::NAME,
        description: index::manifest::DESCRIPTION,
        describe: index_descriptor,
        handle: index_answer,
    },
];

/// The tool `name` denotes, or `None` for a token that names no tool.
#[must_use]
pub fn find(name: &str) -> Option<&'static RegisteredTool> {
    TOOLS.iter().find(|tool| tool.name == name)
}

// -------------------------------------------------------------------------------------------
// `status`: three optional arguments, no bounds, no enum; one list example.
// -------------------------------------------------------------------------------------------

fn status_descriptor() -> Descriptor {
    Descriptor {
        params: status::manifest::PARAMETERS
            .iter()
            .map(|param| RegisteredParam {
                name: param.name,
                kind: match param.ty {
                    status::manifest::ParamType::Str => Kind::Str,
                    status::manifest::ParamType::Integer => Kind::Integer,
                    status::manifest::ParamType::Array(values) => Kind::Array(items_of(values)),
                },
                required: param.required,
                default: param.default.map(|default| match default {
                    status::manifest::ParamDefault::Integer(number) => Default::Integer(number),
                    status::manifest::ParamDefault::List(values) => Default::List(values),
                }),
                // `status` declares no bounds: its only integer is a token budget with no floor.
                min: None,
                max: None,
                description: param.description,
            })
            .collect(),
        example: Some(example_pairs(
            &status::manifest::EXAMPLE,
            |value| match value {
                StatusExample::List(values) => Example::List(values),
            },
        )),
    }
}

fn status_answer(ctx: &ToolContext, args: &Args) -> Envelope<Value> {
    status::handler::handle(
        ctx,
        &status::manifest::StatusArgs {
            paths: strings_of(args, "paths"),
            budget: integer_of(args, "budget"),
        },
    )
}

// -------------------------------------------------------------------------------------------
// `summary`: an enum, a number and its bounds; two text example fields.
// -------------------------------------------------------------------------------------------

fn summary_descriptor() -> Descriptor {
    Descriptor {
        params: summary::manifest::PARAMETERS
            .iter()
            .map(|param| RegisteredParam {
                name: param.name,
                kind: match param.ty {
                    summary::manifest::ParamType::Str => Kind::Str,
                    summary::manifest::ParamType::Integer => Kind::Integer,
                    summary::manifest::ParamType::Enum(values) => Kind::Enum(values),
                    summary::manifest::ParamType::Array(values) => Kind::Array(items_of(values)),
                },
                required: param.required,
                default: param.default.map(|default| match default {
                    summary::manifest::ParamDefault::Text(text) => Default::Text(text),
                    summary::manifest::ParamDefault::Integer(number) => Default::Integer(number),
                }),
                min: param.min,
                max: param.max,
                description: param.description,
            })
            .collect(),
        example: Some(example_pairs(
            &summary::manifest::EXAMPLE,
            |value| match value {
                SummaryExample::Text(text) => Example::Text(text),
                SummaryExample::Integer(number) => Example::Integer(number),
            },
        )),
    }
}

fn summary_answer(ctx: &ToolContext, args: &Args) -> Envelope<Value> {
    summary::handler::handle(
        ctx,
        &summary::manifest::SummaryArgs {
            scope: text_of(args, "scope").as_deref().and_then(scope_of),
            target: text_of(args, "target"),
            base: text_of(args, "base"),
            budget: tokens_of(args, "budget"),
            cursor: text_of(args, "cursor"),
            repo: text_of(args, "repo"),
        },
    )
}

// -------------------------------------------------------------------------------------------
// `trace`: the richest table, nine arguments; an array of enums, two enum fields, bounds.
// -------------------------------------------------------------------------------------------

fn trace_descriptor() -> Descriptor {
    Descriptor {
        params: trace::manifest::PARAMETERS
            .iter()
            .map(|param| RegisteredParam {
                name: param.name,
                kind: match param.ty {
                    trace::manifest::ParamType::Str => Kind::Str,
                    trace::manifest::ParamType::Integer => Kind::Integer,
                    trace::manifest::ParamType::Enum(values) => Kind::Enum(values),
                    trace::manifest::ParamType::Array(values) => Kind::Array(items_of(values)),
                },
                required: param.required,
                default: param.default.map(|default| match default {
                    trace::manifest::ParamDefault::Text(text) => Default::Text(text),
                    trace::manifest::ParamDefault::Integer(number) => Default::Integer(number),
                }),
                min: param.min,
                max: param.max,
                description: param.description,
            })
            .collect(),
        example: Some(example_pairs(
            &trace::manifest::EXAMPLE,
            |value| match value {
                TraceExample::Text(text) => Example::Text(text),
                TraceExample::Integer(number) => Example::Integer(number),
            },
        )),
    }
}

fn trace_answer(ctx: &ToolContext, args: &Args) -> Envelope<Value> {
    trace::handler::handle(
        ctx,
        &trace::manifest::TraceArgs {
            // Declared required, and the parser refuses a call without it, so the fallback describes
            // a call that cannot arrive rather than a default: an empty query resolves to nothing.
            query: text_of(args, "query").unwrap_or_default(),
            direction: text_of(args, "direction").as_deref().and_then(direction_of),
            kinds: strings_of(args, "kinds"),
            path: text_of(args, "path"),
            depth: integer_of(args, "depth"),
            budget: tokens_of(args, "budget"),
            unresolved: text_of(args, "unresolved").as_deref().and_then(bucket_of),
            cursor: text_of(args, "cursor"),
            repo: text_of(args, "repo"),
        },
    )
}

// -------------------------------------------------------------------------------------------
// `search`: one enum, one required string, no array; two text example fields.
// -------------------------------------------------------------------------------------------

fn search_descriptor() -> Descriptor {
    Descriptor {
        params: search::manifest::PARAMETERS
            .iter()
            .map(|param| RegisteredParam {
                name: param.name,
                kind: match param.ty {
                    search::manifest::ParamType::Str => Kind::Str,
                    search::manifest::ParamType::Integer => Kind::Integer,
                    search::manifest::ParamType::Enum(values) => Kind::Enum(values),
                },
                required: param.required,
                default: param.default.map(|default| match default {
                    search::manifest::ParamDefault::Integer(number) => Default::Integer(number),
                }),
                min: param.min,
                max: param.max,
                description: param.description,
            })
            .collect(),
        example: Some(example_pairs(
            &search::manifest::EXAMPLE,
            |value| match value {
                SearchExample::Text(text) => Example::Text(text),
            },
        )),
    }
}

fn search_answer(ctx: &ToolContext, args: &Args) -> Envelope<Value> {
    payload_as_value(search::handler::handle(
        ctx,
        &search::manifest::SearchArgs {
            query: text_of(args, "query").unwrap_or_default(),
            kind: text_of(args, "kind"),
            path: text_of(args, "path"),
            budget: integer_of(args, "budget"),
            cursor: text_of(args, "cursor"),
            repo: text_of(args, "repo"),
        },
    ))
}

// -------------------------------------------------------------------------------------------
// `impact`: a string array with no value set, bounds, no enum; one integer example field.
// -------------------------------------------------------------------------------------------

fn impact_descriptor() -> Descriptor {
    Descriptor {
        params: impact::manifest::PARAMETERS
            .iter()
            .map(|param| RegisteredParam {
                name: param.name,
                kind: match param.ty {
                    impact::manifest::ParamType::Str => Kind::Str,
                    impact::manifest::ParamType::Integer => Kind::Integer,
                    impact::manifest::ParamType::Enum(values) => Kind::Enum(values),
                    impact::manifest::ParamType::Array(values) => Kind::Array(items_of(values)),
                },
                required: param.required,
                default: param.default.map(|default| match default {
                    impact::manifest::ParamDefault::Text(text) => Default::Text(text),
                    impact::manifest::ParamDefault::Integer(number) => Default::Integer(number),
                    impact::manifest::ParamDefault::List(values) => Default::List(values),
                }),
                min: param.min,
                max: param.max,
                description: param.description,
            })
            .collect(),
        example: Some(example_pairs(
            &impact::manifest::EXAMPLE,
            |value| match value {
                ImpactExample::Text(text) => Example::Text(text),
                ImpactExample::Integer(number) => Example::Integer(number),
            },
        )),
    }
}

fn impact_answer(ctx: &ToolContext, args: &Args) -> Envelope<Value> {
    impact::handler::handle(
        ctx,
        &impact::manifest::ImpactArgs {
            paths: strings_of(args, "paths"),
            base: text_of(args, "base"),
            depth: integer_of(args, "depth"),
            budget: tokens_of(args, "budget"),
            cursor: text_of(args, "cursor"),
            repo: text_of(args, "repo"),
        },
    )
}

// -------------------------------------------------------------------------------------------
// `expand`: the one integer array (`range`), one enum, bounds; two text example fields.
// -------------------------------------------------------------------------------------------

fn expand_descriptor() -> Descriptor {
    Descriptor {
        params: expand::manifest::PARAMETERS
            .iter()
            .map(|param| RegisteredParam {
                name: param.name,
                kind: match param.ty {
                    expand::manifest::ParamType::Str => Kind::Str,
                    expand::manifest::ParamType::Integer => Kind::Integer,
                    expand::manifest::ParamType::Enum(values) => Kind::Enum(values),
                    // `range` is `[from, to]`: an array of integers, not of strings.
                    expand::manifest::ParamType::IntegerArray => Kind::Array(Items::Integer),
                },
                required: param.required,
                default: param.default.map(|default| match default {
                    expand::manifest::ParamDefault::Text(text) => Default::Text(text),
                    expand::manifest::ParamDefault::Integer(number) => Default::Integer(number),
                }),
                min: param.min,
                max: param.max,
                description: param.description,
            })
            .collect(),
        example: Some(example_pairs(
            &expand::manifest::EXAMPLE,
            |value| match value {
                ExpandExample::Text(text) => Example::Text(text),
            },
        )),
    }
}

fn expand_answer(ctx: &ToolContext, args: &Args) -> Envelope<Value> {
    expand::handler::handle(
        ctx,
        &expand::manifest::ExpandArgs {
            target: text_of(args, "target").unwrap_or_default(),
            what: text_of(args, "what")
                .as_deref()
                .and_then(expand::manifest::What::parse),
            path: text_of(args, "path"),
            range: integers_of(args, "range"),
            budget: tokens_of(args, "budget"),
            repo: text_of(args, "repo"),
        },
    )
}

// -------------------------------------------------------------------------------------------
// `index`: two booleans, one enum, no bounds; one text example field.
// -------------------------------------------------------------------------------------------

fn index_descriptor() -> Descriptor {
    Descriptor {
        params: index::manifest::PARAMETERS
            .iter()
            .map(|param| RegisteredParam {
                name: param.name,
                kind: match param.ty {
                    index::manifest::ParamType::Str => Kind::Str,
                    index::manifest::ParamType::Enum(values) => Kind::Enum(values),
                    index::manifest::ParamType::Bool => Kind::Bool,
                },
                required: param.required,
                default: param.default.map(|default| match default {
                    index::manifest::ParamDefault::Text(text) => Default::Text(text),
                    index::manifest::ParamDefault::Bool(flag) => Default::Bool(flag),
                }),
                min: param.min,
                max: param.max,
                description: param.description,
            })
            .collect(),
        example: Some(example_pairs(
            &index::manifest::EXAMPLE,
            |value| match value {
                IndexExample::Text(text) => Example::Text(text),
            },
        )),
    }
}

fn index_answer(ctx: &ToolContext, args: &Args) -> Envelope<Value> {
    payload_as_value(index::handler::handle(
        ctx,
        &index::manifest::IndexArgs {
            force: boolean_of(args, "force"),
            mode: text_of(args, "mode")
                .as_deref()
                .and_then(index::manifest::IndexMode::parse),
            repo: text_of(args, "repo"),
            register: boolean_of(args, "register"),
        },
    ))
}

// -------------------------------------------------------------------------------------------
// The shared conversions: the argument map, and one envelope payload type to `Value`.
// -------------------------------------------------------------------------------------------

/// One manifest's `(key, value)` example list in the shared example vocabulary.
fn example_pairs<V: Copy>(
    example: &[(&'static str, V)],
    convert: impl Fn(V) -> Example,
) -> Vec<(&'static str, Example)> {
    example
        .iter()
        .map(|(name, value)| (*name, convert(*value)))
        .collect()
}

/// A declared string argument, or `None` when the call left it out.
fn text_of(args: &Args, name: &str) -> Option<String> {
    args.get(name).and_then(Value::as_str).map(str::to_owned)
}

/// A declared integer argument. The parser stored a validated integer as an integer.
fn integer_of(args: &Args, name: &str) -> Option<i64> {
    args.get(name).and_then(Value::as_i64)
}

/// A declared switch.
fn boolean_of(args: &Args, name: &str) -> Option<bool> {
    args.get(name).and_then(Value::as_bool)
}

/// A declared string list, or `None` when the call left it out.
///
/// A non-string item is dropped rather than coerced: the parser hands an array over in its declared
/// item type, so an item this skips is a shape only a direct in-process call can produce.
fn strings_of(args: &Args, name: &str) -> Option<Vec<String>> {
    args.get(name).and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

/// A declared integer list (`expand`'s `range`). Same rule as [`strings_of`]: declared items only.
fn integers_of(args: &Args, name: &str) -> Option<Vec<i64>> {
    args.get(name)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_i64).collect::<Vec<i64>>())
}

/// A declared token ceiling, as the `u64` the three `budget` fields use.
///
/// A negative number has no `u64` spelling and becomes `None`, which is the declared default: the
/// schema says `integer`, and the handlers' own contract is a ceiling, so a negative ceiling cannot
/// describe a call this tool answers.
fn tokens_of(args: &Args, name: &str) -> Option<u64> {
    integer_of(args, name).and_then(|tokens| u64::try_from(tokens).ok())
}

/// The declared `scope` value as its enum. A value outside the set was refused by the parser, and
/// `None` here is the declared default (`change`), so this is a total function over what can arrive.
fn scope_of(text: &str) -> Option<summary::manifest::Scope> {
    summary::manifest::Scope::ALL
        .into_iter()
        .find(|scope| scope.as_str() == text)
}

/// The declared `direction` value as its enum; `None` is the declared default (`all`).
fn direction_of(text: &str) -> Option<sangkar_domain::neighbors::Direction> {
    use sangkar_domain::neighbors::Direction;

    [Direction::All, Direction::In, Direction::Out]
        .into_iter()
        .find(|direction| direction.as_str() == text)
}

/// The declared `unresolved` bucket value; `None` means "no filter", which is the absent default.
fn bucket_of(text: &str) -> Option<sangkar_domain::unresolved::UnresolvedBucket> {
    sangkar_domain::unresolved::UnresolvedBucket::ALL
        .into_iter()
        .find(|bucket| bucket.as_str() == text)
}

/// One tool's typed payload as the one `Value` the surface renders.
///
/// `search` and `index` answer with their own payload structs rather than a `Value`; the wire shape
/// is the JSON those structs serialise to, so the conversion is the serialisation and the field order
/// stays the struct's own declaration order (`preserve_order`).
fn payload_as_value<T: Serialize>(envelope: Envelope<T>) -> Envelope<Value> {
    let Envelope {
        status,
        summary,
        read_status,
        coverage,
        totals,
        truncated,
        truncation_reason,
        next,
        next_tools,
        payload,
        error,
    } = envelope;
    Envelope {
        status,
        summary,
        read_status,
        coverage,
        totals,
        truncated,
        truncation_reason,
        next,
        next_tools,
        // Every field of both payload types is a string, an integer, an enum or a list of those, so
        // serialisation cannot fail; a `null` is the renderer's honest answer if it ever does,
        // rather than a panic inside a tool call.
        payload: payload.map(|payload| serde_json::to_value(payload).unwrap_or(Value::Null)),
        error,
    }
}
