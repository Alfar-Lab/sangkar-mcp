//! The bind pass on hand-built unit sets: binding is a function of qns and names, so the extractor's
//! grammar work is proved in `tests/extract_sites.rs` and what is under test here is the binding
//! order, the tiers it assigns, and the edges it refuses to emit.
//!
//! Ported from `tests/resolve.test.ts`, fixture for fixture. The main graph is deliberately six small
//! files: `render` occurs twice in the index (`Base.render`, `Panel.render`) so the unique-name tier
//! cannot answer for it and the superclass tier has to, `Widget` both inherits `Base` and implements
//! `Reader`, `Config` is unique so a reference to it binds, while a reference to `render` is
//! ambiguous and must disappear without a trace.
//!
//! Three more graphs pin the parts the main one cannot: `TIE_UNITS` for the ambiguity check at each
//! of the three steps that has one, `IMPORT_UNITS` for the four causes a failed import can have, and
//! `RELATIVE_UNITS` for the importer-relative reading of a `./` specifier.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use sangkar_core::paths::qn;
use sangkar_index::extract::{
    EXTRACTED_CANDIDATE, ExtractedNode, NodeProps, RawEdge, UnresolvedSite,
};
use sangkar_index::resolve::{
    BIND_EVIDENCE_KINDS, BindResult, EDGE_TIERS, FileUnit, ResolvedEdge, SymbolIndex,
    UnresolvedBinding, bind,
};

const BASE_FILE: &str = "src/base.py::base.py";
const BASE_MODULE: &str = "src/base.py::base";
const BASE: &str = "src/base.py::Base";
const BASE_RENDER: &str = "src/base.py::Base.render";

const PANEL_MODULE: &str = "src/panel.py::panel";
const PANEL: &str = "src/panel.py::Panel";
const PANEL_RENDER: &str = "src/panel.py::Panel.render";

const HELPERS_FILE: &str = "src/helpers.py::helpers.py";
const HELPERS_MODULE: &str = "src/helpers.py::helpers";
const NORMALISE: &str = "src/helpers.py::normalise";

const UTIL_MODULE: &str = "src/util.py::util";
const UTIL_UNIQUE: &str = "src/util.py::uniqueHelper";
const UTIL_CONFIG: &str = "src/util.py::Config";

const PORTS_MODULE: &str = "src/ports.py::ports";
const READER: &str = "src/ports.py::Reader";

const WIDGET_FILE: &str = "src/widget.py::widget.py";
const WIDGET_MODULE: &str = "src/widget.py::widget";
const WIDGET: &str = "src/widget.py::Widget";
const WIDGET_PAINT: &str = "src/widget.py::Widget.paint";
const WIDGET_DRAW: &str = "src/widget.py::Widget.draw";

/// The extra fields a raw edge may carry, mirroring the TypeScript `RawSpec` argument.
#[derive(Default)]
struct Spec<'a> {
    receiver: Option<&'a str>,
    local_name: &'a str,
    evidence: Option<&'a str>,
}

/// One declaration. `start_line` is the line the assertions below refer to.
fn decl(kind: &str, path: &str, scope: &str, name: &str, start_line: u32) -> ExtractedNode {
    ExtractedNode {
        kind: kind.to_owned(),
        name: name.to_owned(),
        qn: qn(path, scope, name),
        start_line,
        end_line: start_line + 2,
        signature: None,
        is_test: false,
        props: NodeProps::new(),
    }
}

fn raw(kind: &str, src: &str, target: &str, line: u32, spec: Spec<'_>) -> RawEdge {
    RawEdge {
        kind: kind.to_owned(),
        src: src.to_owned(),
        target: target.to_owned(),
        receiver: spec.receiver.map(str::to_owned),
        line,
        tier: EXTRACTED_CANDIDATE.to_owned(),
        evidence: Some(spec.evidence.unwrap_or(kind).to_owned()),
        local_name: spec.local_name.to_owned(),
    }
}

fn contains(parent: &str, child: &str, line: u32) -> RawEdge {
    raw(
        "contains",
        parent,
        child,
        line,
        Spec {
            evidence: Some("contains"),
            ..Spec::default()
        },
    )
}

fn call(src: &str, target: &str, line: u32, receiver: Option<&str>, evidence: &str) -> RawEdge {
    raw(
        "calls",
        src,
        target,
        line,
        Spec {
            receiver,
            evidence: Some(evidence),
            ..Spec::default()
        },
    )
}

fn unit(
    rel_path: &str,
    nodes: Vec<ExtractedNode>,
    raw_edges: Vec<RawEdge>,
    unresolved: Vec<UnresolvedSite>,
) -> FileUnit {
    FileUnit {
        rel_path: rel_path.to_owned(),
        nodes,
        raw_edges,
        unresolved,
    }
}

/// One graph plus the two answers derived from it, so a fixture is built once per test binary.
struct Fixture {
    units: Vec<FileUnit>,
    index: SymbolIndex,
    result: BindResult,
}

impl Fixture {
    fn new(units: Vec<FileUnit>) -> Self {
        let index = SymbolIndex::from(units.as_slice());
        let result = bind(&units, &index);
        Self {
            units,
            index,
            result,
        }
    }

    fn edges_of_kind(&self, kind: &str) -> Vec<&ResolvedEdge> {
        self.result
            .edges
            .iter()
            .filter(|edge| edge.kind == kind)
            .collect()
    }

    fn call_at(&self, line: u32) -> Option<&ResolvedEdge> {
        self.result
            .edges
            .iter()
            .find(|edge| edge.line == line && edge.kind == "calls")
    }

    fn unresolved_at(&self, line: u32) -> Option<&UnresolvedBinding> {
        self.result
            .unresolved
            .iter()
            .find(|entry| entry.line == line)
    }
}

fn main_units() -> Vec<FileUnit> {
    vec![
        unit(
            "src/base.py",
            vec![
                decl("file", "src/base.py", "", "base.py", 1),
                decl("module", "src/base.py", "", "base", 1),
                decl("class", "src/base.py", "", "Base", 3),
                decl("method", "src/base.py", "Base", "render", 4),
            ],
            vec![
                contains(BASE_FILE, BASE_MODULE, 1),
                contains(BASE_MODULE, BASE, 3),
                contains(BASE, BASE_RENDER, 4),
            ],
            Vec::new(),
        ),
        unit(
            "src/panel.py",
            vec![
                decl("file", "src/panel.py", "", "panel.py", 1),
                decl("module", "src/panel.py", "", "panel", 1),
                decl("class", "src/panel.py", "", "Panel", 3),
                decl("method", "src/panel.py", "Panel", "render", 4),
            ],
            vec![
                contains("src/panel.py::panel.py", PANEL_MODULE, 1),
                contains(PANEL_MODULE, PANEL, 3),
                contains(PANEL, PANEL_RENDER, 4),
            ],
            Vec::new(),
        ),
        unit(
            "src/helpers.py",
            vec![
                decl("file", "src/helpers.py", "", "helpers.py", 1),
                decl("module", "src/helpers.py", "", "helpers", 1),
                decl("function", "src/helpers.py", "", "normalise", 2),
            ],
            vec![
                contains(HELPERS_FILE, HELPERS_MODULE, 1),
                contains(HELPERS_MODULE, NORMALISE, 2),
            ],
            Vec::new(),
        ),
        unit(
            "src/util.py",
            vec![
                decl("file", "src/util.py", "", "util.py", 1),
                decl("module", "src/util.py", "", "util", 1),
                decl("function", "src/util.py", "", "uniqueHelper", 2),
                decl("class", "src/util.py", "", "Config", 5),
            ],
            vec![
                contains("src/util.py::util.py", UTIL_MODULE, 1),
                contains(UTIL_MODULE, UTIL_UNIQUE, 2),
                contains(UTIL_MODULE, UTIL_CONFIG, 5),
            ],
            Vec::new(),
        ),
        unit(
            "src/ports.py",
            vec![
                decl("file", "src/ports.py", "", "ports.py", 1),
                decl("module", "src/ports.py", "", "ports", 1),
                decl("class", "src/ports.py", "", "Reader", 3),
            ],
            vec![
                contains("src/ports.py::ports.py", PORTS_MODULE, 1),
                contains(PORTS_MODULE, READER, 3),
            ],
            Vec::new(),
        ),
        unit(
            "src/widget.py",
            vec![
                decl("file", "src/widget.py", "", "widget.py", 1),
                decl("module", "src/widget.py", "", "widget", 1),
                decl("class", "src/widget.py", "", "Widget", 5),
                decl("method", "src/widget.py", "Widget", "paint", 9),
                decl("method", "src/widget.py", "Widget", "draw", 12),
            ],
            vec![
                contains(WIDGET_FILE, WIDGET_MODULE, 1),
                contains(WIDGET_MODULE, WIDGET, 5),
                contains(WIDGET, WIDGET_PAINT, 9),
                contains(WIDGET, WIDGET_DRAW, 12),
                // Non-call edges: one resolvable base, one interface, two imports.
                raw(
                    "inherits",
                    WIDGET,
                    "Base",
                    5,
                    Spec {
                        evidence: Some("inherits"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "implements",
                    WIDGET,
                    "Reader",
                    5,
                    Spec {
                        evidence: Some("implements"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    WIDGET_MODULE,
                    "helpers",
                    2,
                    Spec {
                        local_name: "normalise",
                        evidence: Some("import:helpers"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    WIDGET_MODULE,
                    "pathlib",
                    3,
                    Spec {
                        local_name: "Path",
                        evidence: Some("import:pathlib"),
                        ..Spec::default()
                    },
                ),
                // The five call-binding cases, in order of the line they sit on.
                call(WIDGET_DRAW, "paint", 10, Some("self"), "call:self.paint"),
                call(WIDGET_DRAW, "normalise", 11, None, "call:normalise"),
                call(WIDGET_DRAW, "uniqueHelper", 12, None, "call:uniqueHelper"),
                call(WIDGET_DRAW, "render", 13, Some("self"), "call:self.render"),
                call(
                    WIDGET_DRAW,
                    "render",
                    14,
                    Some("widget"),
                    "call:widget.render",
                ),
                call(WIDGET_DRAW, "render", 15, None, "call:render"),
                call(WIDGET_DRAW, "mystery", 16, None, "call:mystery"),
                // One reference that binds, one ambiguous, one unmatched.
                raw(
                    "references",
                    WIDGET_DRAW,
                    "Config",
                    17,
                    Spec {
                        evidence: Some("assignment_rhs"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "references",
                    WIDGET_DRAW,
                    "render",
                    18,
                    Spec {
                        evidence: Some("array_element"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "references",
                    WIDGET_DRAW,
                    "absent",
                    19,
                    Spec {
                        evidence: Some("return_expression"),
                        ..Spec::default()
                    },
                ),
            ],
            vec![UnresolvedSite {
                line: 20,
                raw_target: "obj[0]".to_owned(),
                evidence_kind: "dynamic".to_owned(),
                evidence: "call".to_owned(),
            }],
        ),
    ]
}

static MAIN: LazyLock<Fixture> = LazyLock::new(|| Fixture::new(main_units()));

/// Every qn the main units declare, the set the no-dangling-edge invariant is checked against.
fn node_qns(units: &[FileUnit]) -> BTreeSet<&str> {
    units
        .iter()
        .flat_map(|unit| unit.nodes.iter().map(|node| node.qn.as_str()))
        .collect()
}

#[test]
fn symbol_index_answers_per_file_declarations_and_import_tables() {
    let main = &*MAIN;
    let widget: Vec<&str> = main
        .index
        .nodes_in("src/widget.py")
        .into_iter()
        .map(|node| node.qn.as_str())
        .collect();
    assert_eq!(
        widget,
        vec![
            WIDGET_FILE,
            WIDGET_MODULE,
            WIDGET,
            WIDGET_PAINT,
            WIDGET_DRAW
        ]
    );

    let bindings = main.index.import_bindings("src/widget.py", "normalise");
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].module, "helpers");
    assert_eq!(bindings[0].local_name, "normalise");
    assert_eq!(
        main.index
            .import_bindings("src/widget.py", "Path")
            .first()
            .map(|binding| binding.module.as_str()),
        Some("pathlib")
    );

    // A module string names a unit; `index`/`__init__` answering for their directory is not used
    // here, so only the plain stem lookup is exercised.
    assert_eq!(
        main.index.module_file("helpers", None).as_deref(),
        Some("src/helpers.py")
    );
    assert_eq!(main.index.module_file("pathlib", None), None);
    assert_eq!(
        main.index.module_candidates("helpers", None),
        vec!["src/helpers.py".to_owned()]
    );
    assert!(main.index.module_candidates("pathlib", None).is_empty());

    // Reportable declarations only: the module node named `widget` is not a call target.
    assert!(main.index.nodes_named("widget").is_empty());
    let mut renders: Vec<&str> = main
        .index
        .nodes_named("render")
        .into_iter()
        .map(|node| node.qn.as_str())
        .collect();
    renders.sort_unstable();
    let mut expected = vec![BASE_RENDER, PANEL_RENDER];
    expected.sort_unstable();
    assert_eq!(renders, expected);
}

#[test]
fn no_edge_targets_a_node_that_does_not_exist_on_either_end() {
    let main = &*MAIN;
    let known = node_qns(&main.units);
    assert!(!main.result.edges.is_empty());
    for edge in &main.result.edges {
        assert!(
            known.contains(edge.dst.as_str()),
            "dangling dst: {} -> {}",
            edge.src,
            edge.dst
        );
        assert!(
            known.contains(edge.src.as_str()),
            "dangling src: {} -> {}",
            edge.src,
            edge.dst
        );
    }
}

#[test]
fn every_edge_names_a_unit_and_a_declared_tier() {
    let main = &*MAIN;
    let paths: BTreeSet<&str> = main
        .units
        .iter()
        .map(|unit| unit.rel_path.as_str())
        .collect();
    for edge in &main.result.edges {
        assert!(
            paths.contains(edge.file.as_str()),
            "unknown unit {}",
            edge.file
        );
        assert!(
            EDGE_TIERS.contains(&edge.tier.as_str()),
            "unknown tier {}",
            edge.tier
        );
    }
    for entry in &main.result.unresolved {
        assert!(
            BIND_EVIDENCE_KINDS.contains(&entry.evidence_kind.as_str()),
            "unknown evidence kind {}",
            entry.evidence_kind
        );
    }
}

#[test]
fn the_pass_is_pure_and_a_second_run_is_identical() {
    let units = main_units();
    let second = bind(&units, &SymbolIndex::from(units.as_slice()));
    assert_eq!(second, MAIN.result);
}

#[test]
fn step_1_an_exact_qn_in_the_same_file_binds_at_extracted() {
    let edge = MAIN.call_at(10).expect("line 10 binds");
    assert_eq!(
        edge,
        &ResolvedEdge {
            file: "src/widget.py".to_owned(),
            kind: "calls".to_owned(),
            src: WIDGET_DRAW.to_owned(),
            dst: WIDGET_PAINT.to_owned(),
            tier: "extracted".to_owned(),
            line: 10,
            local_name: String::new(),
            evidence: "call:self.paint".to_owned(),
        }
    );
}

#[test]
fn step_2_a_name_visible_through_the_import_table_binds_at_extracted() {
    let edge = MAIN.call_at(11).expect("line 11 binds");
    assert_eq!(
        (edge.src.as_str(), edge.dst.as_str(), edge.tier.as_str()),
        (WIDGET_DRAW, NORMALISE, "extracted")
    );
}

#[test]
fn step_3_a_name_unique_in_the_whole_index_binds_at_inferred() {
    let edge = MAIN.call_at(12).expect("line 12 binds");
    assert_eq!(
        (edge.src.as_str(), edge.dst.as_str(), edge.tier.as_str()),
        (WIDGET_DRAW, UTIL_UNIQUE, "inferred")
    );

    // Only reachable through step 2 because `normalise` is imported: the same call without the
    // import statement falls through to the unique-name tier, which is what makes that tier
    // observable.
    let stripped: Vec<FileUnit> = main_units()
        .into_iter()
        .map(|entry| {
            if entry.rel_path != "src/widget.py" {
                return entry;
            }
            let kept: Vec<RawEdge> = entry
                .raw_edges
                .into_iter()
                .filter(|edge| edge.kind != "imports")
                .collect();
            unit(&entry.rel_path, entry.nodes, kept, entry.unresolved)
        })
        .collect();
    let no_imports = bind(&stripped, &SymbolIndex::from(stripped.as_slice()));
    let fallback = no_imports
        .edges
        .iter()
        .find(|edge| edge.line == 11 && edge.kind == "calls")
        .expect("line 11 still binds by unique name");
    assert_eq!(
        (fallback.dst.as_str(), fallback.tier.as_str()),
        (NORMALISE, "inferred")
    );
}

#[test]
fn step_4_a_superclass_method_binds_at_inferred_for_a_typed_receiver() {
    let edge = MAIN.call_at(13).expect("line 13 binds");
    assert_eq!(
        (edge.src.as_str(), edge.dst.as_str(), edge.tier.as_str()),
        (WIDGET_DRAW, BASE_RENDER, "inferred")
    );
    // The name alone is ambiguous (`Base.render` and `Panel.render`), so this edge can only come
    // from the inheritance evidence.
    assert_eq!(MAIN.index.nodes_named("render").len(), 2);
    let call_targets: Vec<&str> = MAIN
        .edges_of_kind("calls")
        .into_iter()
        .map(|edge| edge.dst.as_str())
        .collect();
    assert!(!call_targets.contains(&PANEL_RENDER));
}

#[test]
fn every_failing_call_is_recorded_with_its_evidence_kind_and_emits_no_edge() {
    // An untypeable receiver: `widget` names no class, so the two candidates cannot be separated.
    assert!(MAIN.call_at(14).is_none());
    assert_eq!(
        MAIN.unresolved_at(14),
        Some(&UnresolvedBinding {
            file: "src/widget.py".to_owned(),
            line: 14,
            raw_target: "render".to_owned(),
            evidence_kind: "receiver_type_unknown".to_owned(),
            evidence: "call:widget.render".to_owned(),
        })
    );

    // No receiver and two candidates: ambiguous.
    assert!(MAIN.call_at(15).is_none());
    assert_eq!(
        MAIN.unresolved_at(15).map(|entry| (
            entry.raw_target.as_str(),
            entry.evidence_kind.as_str(),
            entry.evidence.as_str()
        )),
        Some(("render", "ambiguous", "call:render"))
    );

    // No receiver and no candidate anywhere: dynamic.
    assert!(MAIN.call_at(16).is_none());
    assert_eq!(
        MAIN.unresolved_at(16)
            .map(|entry| (entry.raw_target.as_str(), entry.evidence_kind.as_str())),
        Some(("mystery", "dynamic"))
    );
}

#[test]
fn the_extractors_own_nameless_sites_pass_through_unchanged() {
    assert_eq!(
        MAIN.unresolved_at(20),
        Some(&UnresolvedBinding {
            file: "src/widget.py".to_owned(),
            line: 20,
            raw_target: "obj[0]".to_owned(),
            evidence_kind: "dynamic".to_owned(),
            evidence: "call".to_owned(),
        })
    );
}

#[test]
fn resolved_inherits_and_implements_edges_are_extracted() {
    let inherits = MAIN.edges_of_kind("inherits");
    assert_eq!(inherits.len(), 1);
    assert_eq!(
        (
            inherits[0].src.as_str(),
            inherits[0].dst.as_str(),
            inherits[0].tier.as_str()
        ),
        (WIDGET, BASE, "extracted")
    );

    let implements = MAIN.edges_of_kind("implements");
    assert_eq!(implements.len(), 1);
    assert_eq!(
        (
            implements[0].src.as_str(),
            implements[0].dst.as_str(),
            implements[0].tier.as_str()
        ),
        (WIDGET, READER, "extracted")
    );
}

#[test]
fn an_import_binds_to_the_target_units_file_node_and_an_out_of_tree_module_does_not() {
    assert_eq!(
        MAIN.edges_of_kind("imports"),
        vec![&ResolvedEdge {
            file: "src/widget.py".to_owned(),
            kind: "imports".to_owned(),
            src: WIDGET_MODULE.to_owned(),
            dst: HELPERS_FILE.to_owned(),
            tier: "extracted".to_owned(),
            line: 2,
            local_name: "normalise".to_owned(),
            evidence: "import:helpers".to_owned(),
        }]
    );
    assert_eq!(
        MAIN.unresolved_at(3)
            .map(|entry| (entry.raw_target.as_str(), entry.evidence_kind.as_str())),
        Some(("pathlib", "dynamic"))
    );
}

#[test]
fn a_reference_binds_at_inferred_only_when_exactly_one_node_matches() {
    assert_eq!(
        MAIN.edges_of_kind("references"),
        vec![&ResolvedEdge {
            file: "src/widget.py".to_owned(),
            kind: "references".to_owned(),
            src: WIDGET_DRAW.to_owned(),
            dst: UTIL_CONFIG.to_owned(),
            tier: "inferred".to_owned(),
            line: 17,
            local_name: String::new(),
            evidence: "assignment_rhs".to_owned(),
        }]
    );
}

#[test]
fn an_ambiguous_or_unmatched_reference_is_dropped_silently_with_no_unresolved_row() {
    let referenced: Vec<&str> = MAIN
        .edges_of_kind("references")
        .into_iter()
        .map(|edge| edge.dst.as_str())
        .collect();
    assert!(!referenced.contains(&BASE_RENDER));
    assert!(!referenced.contains(&PANEL_RENDER));
    assert!(
        !MAIN
            .result
            .edges
            .iter()
            .any(|edge| edge.kind == "references" && edge.line == 18)
    );
    assert!(
        !MAIN
            .result
            .edges
            .iter()
            .any(|edge| edge.kind == "references" && edge.line == 19)
    );

    // Silence is the contract: a non-unique name is not an unresolvable site.
    assert!(MAIN.unresolved_at(18).is_none());
    assert!(MAIN.unresolved_at(19).is_none());
    assert!(
        !MAIN
            .result
            .unresolved
            .iter()
            .any(|entry| entry.evidence == "array_element")
    );
    assert!(
        !MAIN
            .result
            .unresolved
            .iter()
            .any(|entry| entry.evidence == "return_expression")
    );
}

#[test]
fn containment_survives_the_pass_at_extracted_once_per_raw_contains_edge() {
    let raw_contains = MAIN
        .units
        .iter()
        .flat_map(|unit| unit.raw_edges.iter())
        .filter(|edge| edge.kind == "contains")
        .count();
    let bound = MAIN.edges_of_kind("contains");
    assert_eq!(bound.len(), raw_contains);
    assert!(bound.iter().all(|edge| edge.tier == "extracted"));
    assert!(bound.iter().any(|edge| edge.src == WIDGET_FILE
        && edge.dst == WIDGET_MODULE
        && edge.tier == "extracted"));
    assert!(
        bound
            .iter()
            .any(|edge| edge.src == WIDGET && edge.dst == WIDGET_DRAW && edge.tier == "extracted")
    );
    // No containment edge was collapsed by the dedupe: the full list is the count. Every other edge
    // in the fixture is one of the eight resolved non-contains edges.
    assert_eq!(MAIN.result.edges.len(), bound.len() + 8);
}

#[test]
fn an_edge_whose_src_or_dst_is_not_a_node_is_dropped_rather_than_emitted() {
    let file = "src/guard.py::guard.py";
    let run = "src/guard.py::run";
    let helper = "src/other.py::helper";
    let units = vec![
        unit(
            "src/guard.py",
            vec![
                decl("file", "src/guard.py", "", "guard.py", 1),
                decl("function", "src/guard.py", "", "run", 2),
            ],
            vec![
                // A containment edge to a declaration no unit made: dropped, and containment has
                // no failure path, so it leaves no trace at all.
                contains(file, "src/guard.py::Ghost", 1),
                // `helper` is unique, so step 3 resolves it — and the guard still drops the edge
                // because the site's own source does not exist.
                call(
                    "src/guard.py::Missing.run",
                    "helper",
                    4,
                    None,
                    "call:helper",
                ),
                call(run, "helper", 5, None, "call:helper"),
            ],
            Vec::new(),
        ),
        unit(
            "src/other.py",
            vec![
                decl("file", "src/other.py", "", "other.py", 1),
                decl("function", "src/other.py", "", "helper", 2),
            ],
            vec![contains("src/other.py::other.py", helper, 2)],
            Vec::new(),
        ),
    ];
    let result = bind(&units, &SymbolIndex::from(units.as_slice()));

    let guard_sites: Vec<&ResolvedEdge> = result
        .edges
        .iter()
        .filter(|edge| edge.src != "src/other.py::other.py")
        .collect();
    assert_eq!(
        guard_sites
            .iter()
            .map(|edge| (edge.kind.as_str(), edge.line))
            .collect::<Vec<_>>(),
        vec![("calls", 5)],
        "the ghost containment and the missing source emitted nothing"
    );
    assert_eq!(guard_sites[0].dst, helper);
    assert!(
        result
            .unresolved
            .iter()
            .all(|entry| entry.file != "src/guard.py"),
        "a dropped edge is not a binding failure"
    );
}

#[test]
fn a_duplicate_site_is_collapsed_on_the_edge_tables_unique_key() {
    let rule = "src/dup.py::rule";
    let check = "src/dup.py::check";
    let units = vec![unit(
        "src/dup.py",
        vec![
            decl("file", "src/dup.py", "", "dup.py", 1),
            decl("function", "src/dup.py", "", "rule", 2),
            decl("function", "src/dup.py", "", "check", 5),
        ],
        vec![
            // The same containment seen twice: one edge.
            contains("src/dup.py::dup.py", rule, 1),
            contains("src/dup.py::dup.py", rule, 1),
            // The same call at the same line, twice: one edge.
            call(check, "rule", 6, None, "call:rule"),
            call(check, "rule", 6, None, "call:rule"),
            // The same call one line later is a different site, so it survives.
            call(check, "rule", 7, None, "call:rule"),
        ],
        Vec::new(),
    )];
    let result = bind(&units, &SymbolIndex::from(units.as_slice()));

    let contained: Vec<&ResolvedEdge> = result
        .edges
        .iter()
        .filter(|edge| edge.kind == "contains" && edge.dst == rule)
        .collect();
    assert_eq!(contained.len(), 1);
    let called: Vec<u32> = result
        .edges
        .iter()
        .filter(|edge| edge.kind == "calls")
        .map(|edge| edge.line)
        .collect();
    assert_eq!(called, vec![6, 7]);
    // One key per edge, and a key built the same way the pass builds it: equal counts is uniqueness.
    let keys: BTreeSet<String> = result
        .edges
        .iter()
        .map(|edge| {
            format!(
                "{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}\u{0}{}",
                edge.file, edge.kind, edge.src, edge.dst, edge.line, edge.local_name
            )
        })
        .collect();
    assert_eq!(keys.len(), result.edges.len());
}

#[test]
fn unresolved_rows_keep_the_contract_order_extractor_sites_first_then_failures() {
    // The widget unit's own site (line 20) comes before its binding failures, and the failures come
    // in raw-edge order — line 3 (an import) precedes the calls at 14, 15 and 16.
    let widget: Vec<(u32, &str)> = MAIN
        .result
        .unresolved
        .iter()
        .filter(|entry| entry.file == "src/widget.py")
        .map(|entry| (entry.line, entry.evidence_kind.as_str()))
        .collect();
    assert_eq!(
        widget,
        vec![
            (20, "dynamic"),
            (3, "dynamic"),
            (14, "receiver_type_unknown"),
            (15, "ambiguous"),
            (16, "dynamic"),
        ]
    );
}

#[test]
fn edges_come_out_in_unit_order_and_then_in_raw_edge_order() {
    // Base is the first unit, so its three containment edges lead.
    let first: Vec<(String, u32)> = MAIN
        .result
        .edges
        .iter()
        .take(3)
        .map(|edge| (edge.kind.clone(), edge.line))
        .collect();
    assert_eq!(
        first,
        vec![
            ("contains".to_owned(), 1),
            ("contains".to_owned(), 3),
            ("contains".to_owned(), 4),
        ]
    );

    // Inside widget.py the sequence is the raw-edge sequence, with the silent references missing.
    let widget: Vec<(String, u32)> = MAIN
        .result
        .edges
        .iter()
        .filter(|edge| edge.file == "src/widget.py")
        .map(|edge| (edge.kind.clone(), edge.line))
        .collect();
    let expected: Vec<(String, u32)> = [
        ("contains", 1),
        ("contains", 5),
        ("contains", 9),
        ("contains", 12),
        ("inherits", 5),
        ("implements", 5),
        ("imports", 2),
        ("calls", 10),
        ("calls", 11),
        ("calls", 12),
        ("calls", 13),
        ("references", 17),
    ]
    .into_iter()
    .map(|(kind, line)| (kind.to_owned(), line))
    .collect();
    assert_eq!(widget, expected);
}

/*
 * The ambiguity check at the three steps that have one, on its own three-file fixture so the main
 * graph above keeps its shape.
 *
 * `go` is inherited under one name from two bases, `build` is both a module function and a method of
 * `Box` in the same file, and `render` is imported from two modules under one local name. Each is a
 * tie the pass must refuse to guess at, and the cause is `ambiguous` in all three.
 */
const TIE_BASE_A: &str = "src/tie.py::A";
const TIE_BASE_B: &str = "src/tie.py::B";
const TIE_D: &str = "src/tie.py::D";
const TIE_D_RUN: &str = "src/tie.py::D.run";
const TIE_LEFT: &str = "src/left.py::render";
const TIE_RIGHT: &str = "src/right.py::render";

fn tie_units() -> Vec<FileUnit> {
    vec![
        unit(
            "src/tie.py",
            vec![
                decl("file", "src/tie.py", "", "tie.py", 1),
                decl("module", "src/tie.py", "", "tie", 1),
                decl("class", "src/tie.py", "", "A", 3),
                decl("method", "src/tie.py", "A", "go", 4),
                decl("class", "src/tie.py", "", "B", 6),
                decl("method", "src/tie.py", "B", "go", 7),
                decl("class", "src/tie.py", "", "D", 9),
                decl("method", "src/tie.py", "D", "run", 10),
                decl("function", "src/tie.py", "", "build", 20),
                decl("class", "src/tie.py", "", "Box", 22),
                decl("method", "src/tie.py", "Box", "build", 23),
            ],
            vec![
                contains("src/tie.py::tie.py", "src/tie.py::tie", 1),
                contains("src/tie.py::tie", TIE_BASE_A, 3),
                contains(TIE_BASE_A, "src/tie.py::A.go", 4),
                contains("src/tie.py::tie", TIE_BASE_B, 6),
                contains(TIE_BASE_B, "src/tie.py::B.go", 7),
                contains("src/tie.py::tie", TIE_D, 9),
                contains(TIE_D, TIE_D_RUN, 10),
                raw(
                    "inherits",
                    TIE_D,
                    "A",
                    9,
                    Spec {
                        evidence: Some("inherits"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "inherits",
                    TIE_D,
                    "B",
                    9,
                    Spec {
                        evidence: Some("inherits"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    "src/tie.py::tie",
                    "left",
                    16,
                    Spec {
                        local_name: "render",
                        evidence: Some("import:left"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    "src/tie.py::tie",
                    "right",
                    17,
                    Spec {
                        local_name: "render",
                        evidence: Some("import:right"),
                        ..Spec::default()
                    },
                ),
                // Step 4: two bases carry `go`, so neither is chosen.
                call(TIE_D_RUN, "go", 11, Some("self"), "call:self.go"),
                // Step 1: two same-file declarations carry `build`, and a receiver was not written.
                call(TIE_D_RUN, "build", 13, None, "call:build"),
                // Step 2: both imported modules define `render`, so the import table ties as well.
                call(TIE_D_RUN, "render", 18, None, "call:render"),
            ],
            Vec::new(),
        ),
        unit(
            "src/left.py",
            vec![
                decl("file", "src/left.py", "", "left.py", 1),
                decl("module", "src/left.py", "", "left", 1),
                decl("function", "src/left.py", "", "render", 2),
            ],
            vec![
                contains("src/left.py::left.py", "src/left.py::left", 1),
                contains("src/left.py::left", TIE_LEFT, 2),
            ],
            Vec::new(),
        ),
        unit(
            "src/right.py",
            vec![
                decl("file", "src/right.py", "", "right.py", 1),
                decl("module", "src/right.py", "", "right", 1),
                decl("function", "src/right.py", "", "render", 2),
            ],
            vec![
                contains("src/right.py::right.py", "src/right.py::right", 1),
                contains("src/right.py::right", TIE_RIGHT, 2),
            ],
            Vec::new(),
        ),
    ]
}

static TIE: LazyLock<Fixture> = LazyLock::new(|| Fixture::new(tie_units()));

#[test]
fn ambiguity_at_step_1_is_recorded_as_ambiguous_and_emits_no_edge() {
    assert!(TIE.call_at(13).is_none());
    let entry = TIE.unresolved_at(13).expect("line 13 is recorded");
    assert_eq!(
        (entry.raw_target.as_str(), entry.evidence_kind.as_str()),
        ("build", "ambiguous")
    );
    assert_eq!(
        TIE.index
            .nodes_in("src/tie.py")
            .into_iter()
            .filter(|node| node.name == "build")
            .count(),
        2
    );
}

#[test]
fn ambiguity_at_step_2_is_recorded_as_ambiguous_and_emits_no_edge() {
    assert!(TIE.call_at(18).is_none());
    let entry = TIE.unresolved_at(18).expect("line 18 is recorded");
    assert_eq!(
        (entry.raw_target.as_str(), entry.evidence_kind.as_str()),
        ("render", "ambiguous")
    );
    assert_eq!(TIE.index.import_bindings("src/tie.py", "render").len(), 2);
}

#[test]
fn ambiguity_at_step_4_is_recorded_as_ambiguous_and_emits_no_edge() {
    assert!(TIE.call_at(11).is_none());
    let entry = TIE.unresolved_at(11).expect("line 11 is recorded");
    assert_eq!(
        (
            entry.raw_target.as_str(),
            entry.evidence_kind.as_str(),
            entry.evidence.as_str()
        ),
        ("go", "ambiguous", "call:self.go")
    );
    // The superclass tier is reached at all only because `go` is not unique: two methods carry it.
    assert_eq!(TIE.index.nodes_named("go").len(), 2);
    assert_eq!(TIE.index.bases_of(TIE_D).len(), 2);
}

/*
 * An inheritance base that does not bind, on its own four-file fixture: one name two types carry,
 * one name no type carries, and one name two types carry *in the same file* — the local count the
 * resolver checks before it ever consults the whole index.
 */
fn type_units() -> Vec<FileUnit> {
    vec![
        unit(
            "src/a.py",
            vec![
                decl("file", "src/a.py", "", "a.py", 1),
                decl("class", "src/a.py", "", "Shared", 2),
            ],
            vec![contains("src/a.py::a.py", "src/a.py::Shared", 2)],
            Vec::new(),
        ),
        unit(
            "src/b.py",
            vec![
                decl("file", "src/b.py", "", "b.py", 1),
                decl("class", "src/b.py", "", "Shared", 2),
            ],
            vec![contains("src/b.py::b.py", "src/b.py::Shared", 2)],
            Vec::new(),
        ),
        unit(
            "src/c.py",
            vec![
                decl("file", "src/c.py", "", "c.py", 1),
                decl("class", "src/c.py", "", "Kid", 2),
            ],
            vec![
                contains("src/c.py::c.py", "src/c.py::Kid", 2),
                raw(
                    "inherits",
                    "src/c.py::Kid",
                    "Shared",
                    2,
                    Spec {
                        evidence: Some("inherits"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "implements",
                    "src/c.py::Kid",
                    "Nowhere",
                    3,
                    Spec {
                        evidence: Some("implements"),
                        ..Spec::default()
                    },
                ),
            ],
            Vec::new(),
        ),
        unit(
            "src/d.py",
            vec![
                decl("file", "src/d.py", "", "d.py", 1),
                decl("class", "src/d.py", "", "Shared", 2),
                decl("class", "src/d.py", "", "Nest", 4),
                decl("class", "src/d.py", "Nest", "Shared", 6),
                decl("class", "src/d.py", "", "Kid2", 9),
            ],
            vec![
                contains("src/d.py::d.py", "src/d.py::Shared", 2),
                contains("src/d.py::d.py", "src/d.py::Nest", 4),
                contains("src/d.py::Nest", "src/d.py::Nest.Shared", 6),
                contains("src/d.py::d.py", "src/d.py::Kid2", 9),
                raw(
                    "inherits",
                    "src/d.py::Kid2",
                    "Shared",
                    9,
                    Spec {
                        evidence: Some("inherits"),
                        ..Spec::default()
                    },
                ),
            ],
            Vec::new(),
        ),
    ]
}

static TYPES: LazyLock<Fixture> = LazyLock::new(|| Fixture::new(type_units()));

#[test]
fn an_inheritance_base_two_types_carry_is_ambiguous() {
    let fixture = &*TYPES;
    assert!(
        fixture
            .result
            .edges
            .iter()
            .all(|edge| edge.kind != "inherits" && edge.kind != "implements"),
        "a base that ties emits no edge"
    );
    // Four classes carry `Shared` across the fixture, and neither of the two in the index alone is
    // chosen: nothing narrows the name to one.
    assert_eq!(fixture.index.nodes_named("Shared").len(), 4);
    assert_eq!(
        fixture.unresolved_at(2),
        Some(&UnresolvedBinding {
            file: "src/c.py".to_owned(),
            line: 2,
            raw_target: "Shared".to_owned(),
            evidence_kind: "ambiguous".to_owned(),
            evidence: "inherits".to_owned(),
        })
    );

    // The same tie decided inside the file itself, before the whole index is consulted: `d.py`
    // declares a top-level `Shared` and a nested one, so the local count alone is already two.
    let local = fixture.unresolved_at(9).expect("line 9 is recorded");
    assert_eq!(
        (
            local.raw_target.as_str(),
            local.evidence_kind.as_str(),
            local.evidence.as_str()
        ),
        ("Shared", "ambiguous", "inherits")
    );
}

#[test]
fn an_inheritance_base_no_type_carries_is_dynamic() {
    let entry = TYPES.unresolved_at(3).expect("line 3 is recorded");
    assert_eq!(
        (
            entry.raw_target.as_str(),
            entry.evidence_kind.as_str(),
            entry.evidence.as_str()
        ),
        ("Nowhere", "dynamic", "implements")
    );
}

/*
 * The import taxonomy: the four causes a failed import can have, on its own six-line fixture so the
 * main graph keeps its shape.
 *
 * `src/one/thing.py` and `src/two/thing.py` share the stem `thing`, so the specifier alone cannot
 * pick one; `src/app.py` imports them and nothing else. A stem tie is a tree defect, a `./` miss is a
 * broken path, a package and a `#` alias are modules this resolver never sees.
 */
const THING_ONE_FILE: &str = "src/one/thing.py::thing.py";
const THING_ONE_MODULE: &str = "src/one/thing.py::thing";
const TWO_THING_FILE: &str = "src/two/thing.py::thing.py";
const TWO_THING_MODULE: &str = "src/two/thing.py::thing";
const APP_FILE: &str = "src/app.py::app.py";
const APP_MODULE: &str = "src/app.py::app";

fn thing_unit(rel_path: &str, file_qn: &str, module_qn: &str) -> FileUnit {
    let name = match rel_path.rfind('/') {
        Some(slash) => &rel_path[slash + 1..],
        None => rel_path,
    };
    unit(
        rel_path,
        vec![
            decl("file", rel_path, "", name, 1),
            decl("module", rel_path, "", "thing", 1),
        ],
        vec![contains(file_qn, module_qn, 1)],
        Vec::new(),
    )
}

fn import_units() -> Vec<FileUnit> {
    vec![
        thing_unit("src/one/thing.py", THING_ONE_FILE, THING_ONE_MODULE),
        thing_unit("src/two/thing.py", TWO_THING_FILE, TWO_THING_MODULE),
        unit(
            "src/app.py",
            vec![
                decl("file", "src/app.py", "", "app.py", 1),
                decl("module", "src/app.py", "", "app", 1),
            ],
            vec![
                contains(APP_FILE, APP_MODULE, 1),
                // A stem tie, a broken relative path, a package, a tsconfig alias, and one import
                // that binds.
                raw(
                    "imports",
                    APP_MODULE,
                    "thing",
                    2,
                    Spec {
                        evidence: Some("import:thing"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    APP_MODULE,
                    "./missing",
                    3,
                    Spec {
                        evidence: Some("import:./missing"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    APP_MODULE,
                    "sangkar-elsewhere",
                    4,
                    Spec {
                        evidence: Some("import:sangkar-elsewhere"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    APP_MODULE,
                    "#shared/thing",
                    5,
                    Spec {
                        evidence: Some("import:#shared/thing"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    APP_MODULE,
                    "./two/thing",
                    6,
                    Spec {
                        evidence: Some("import:./two/thing"),
                        ..Spec::default()
                    },
                ),
            ],
            Vec::new(),
        ),
    ]
}

static IMPORT_FIXTURE: LazyLock<Fixture> = LazyLock::new(|| Fixture::new(import_units()));

#[test]
fn module_candidates_lists_every_unit_a_stem_could_name_sorted() {
    let index = &IMPORT_FIXTURE.index;
    assert_eq!(
        index.module_candidates("thing", None),
        vec!["src/one/thing.py".to_owned(), "src/two/thing.py".to_owned()]
    );
    // Two candidates cannot be resolved, so the coarse answer stays `None` rather than guessing one.
    assert_eq!(index.module_file("thing", None), None);

    // A specifier with a path segment narrows the candidates to one, and then both agree again.
    assert_eq!(
        index.module_candidates("./two/thing", None),
        vec!["src/two/thing.py".to_owned()]
    );
    assert_eq!(
        index.module_file("./two/thing", None).as_deref(),
        Some("src/two/thing.py")
    );

    // A module nothing matches is an empty list, not a null string.
    assert!(index.module_candidates("nowhere", None).is_empty());
    assert_eq!(index.module_file("nowhere", None), None);
}

#[test]
fn a_stem_tie_is_ambiguous_and_names_both_competing_paths_in_its_evidence() {
    let fixture = &*IMPORT_FIXTURE;
    assert!(
        !fixture
            .result
            .edges
            .iter()
            .any(|edge| edge.kind == "imports" && edge.line == 2)
    );
    assert_eq!(
        fixture
            .result
            .unresolved
            .iter()
            .find(|entry| entry.line == 2),
        Some(&UnresolvedBinding {
            file: "src/app.py".to_owned(),
            line: 2,
            raw_target: "thing".to_owned(),
            evidence_kind: "ambiguous".to_owned(),
            evidence: "import:thing candidates: src/one/thing.py | src/two/thing.py".to_owned(),
        })
    );
}

#[test]
fn a_relative_specifier_that_matched_nothing_is_missing_module() {
    assert_eq!(
        IMPORT_FIXTURE
            .result
            .unresolved
            .iter()
            .find(|entry| entry.line == 3),
        Some(&UnresolvedBinding {
            file: "src/app.py".to_owned(),
            line: 3,
            raw_target: "./missing".to_owned(),
            evidence_kind: "missing_module".to_owned(),
            evidence: "import:./missing (no file matched)".to_owned(),
        })
    );
}

#[test]
fn a_bare_package_specifier_stays_dynamic_with_its_evidence_unchanged() {
    let entry = IMPORT_FIXTURE
        .result
        .unresolved
        .iter()
        .find(|entry| entry.line == 4)
        .expect("line 4 is recorded");
    assert_eq!(
        (
            entry.raw_target.as_str(),
            entry.evidence_kind.as_str(),
            entry.evidence.as_str()
        ),
        ("sangkar-elsewhere", "dynamic", "import:sangkar-elsewhere")
    );
}

#[test]
fn a_hash_alias_stays_dynamic_because_a_path_map_is_not_a_defect_in_this_tree() {
    let entry = IMPORT_FIXTURE
        .result
        .unresolved
        .iter()
        .find(|entry| entry.line == 5)
        .expect("line 5 is recorded");
    assert_eq!(
        (
            entry.raw_target.as_str(),
            entry.evidence_kind.as_str(),
            entry.evidence.as_str()
        ),
        ("#shared/thing", "dynamic", "import:#shared/thing")
    );
}

#[test]
fn the_one_import_with_a_single_candidate_binds_to_its_file_node() {
    let imports: Vec<&ResolvedEdge> = IMPORT_FIXTURE
        .result
        .edges
        .iter()
        .filter(|edge| edge.kind == "imports")
        .collect();
    assert_eq!(imports.len(), 1);
    assert_eq!(
        (
            imports[0].src.as_str(),
            imports[0].dst.as_str(),
            imports[0].tier.as_str(),
            imports[0].line
        ),
        (APP_MODULE, TWO_THING_FILE, "extracted", 6)
    );
    assert!(
        IMPORT_FIXTURE
            .result
            .unresolved
            .iter()
            .all(|entry| entry.line != 6)
    );
}

/*
 * Relative specifiers, on a TypeScript tree because `./x.js` naming `x.ts` is the shape an import
 * takes there. `src/a/thing.ts` and `src/other/thing.ts` share the stem `thing`, so only the
 * importer's own directory can separate them; `src/outside.ts` sits at the root so an escaping
 * `../../../outside` has a real unit of that name it must still fail to bind.
 */
const REL_B_MODULE: &str = "src/a/b.ts::b";
const REL_B_RUN: &str = "src/a/b.ts::run";
const REL_AX_FILE: &str = "src/a/x.ts::x.ts";
const REL_AX_HELPER: &str = "src/a/x.ts::helper";
const REL_DIR_FILE: &str = "src/a/dir/index.ts::index.ts";
const REL_THING_FILE: &str = "src/a/thing.ts::thing.ts";
const REL_X_FILE: &str = "src/x.ts::x.ts";

/// `index.ts` → `index`, the module name a file node's sibling carries.
fn stem_of(name: &str) -> &str {
    match name.rfind('.') {
        Some(dot) if dot > 0 && !name[dot + 1..].contains('/') => &name[..dot],
        _ => name,
    }
}

/// A unit carrying the two nodes every real file has — its file node and its module node.
fn source_unit(rel_path: &str, raw_edges: Vec<RawEdge>, nodes: Vec<ExtractedNode>) -> FileUnit {
    let name = match rel_path.rfind('/') {
        Some(slash) => &rel_path[slash + 1..],
        None => rel_path,
    };
    let stem = stem_of(name);
    let file_qn = format!("{rel_path}::{name}");
    let module_qn = format!("{rel_path}::{stem}");
    let mut all = vec![
        decl("file", rel_path, "", name, 1),
        decl("module", rel_path, "", stem, 1),
    ];
    all.extend(nodes);
    let mut edges = vec![contains(&file_qn, &module_qn, 1)];
    edges.extend(raw_edges);
    unit(rel_path, all, edges, Vec::new())
}

fn relative_units() -> Vec<FileUnit> {
    vec![
        source_unit(
            "src/a/b.ts",
            vec![
                // One relative specifier per rule the importer-relative lookup has to get right.
                raw(
                    "imports",
                    REL_B_MODULE,
                    "./x.js",
                    2,
                    Spec {
                        local_name: "helper",
                        evidence: Some("import:./x.js"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    REL_B_MODULE,
                    "./dir/index.js",
                    3,
                    Spec {
                        local_name: "dirIndex",
                        evidence: Some("import:./dir/index.js"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    REL_B_MODULE,
                    "./thing.js",
                    4,
                    Spec {
                        local_name: "thing",
                        evidence: Some("import:./thing.js"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    REL_B_MODULE,
                    "../x.js",
                    5,
                    Spec {
                        local_name: "parent",
                        evidence: Some("import:../x.js"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    REL_B_MODULE,
                    "./nope.js",
                    6,
                    Spec {
                        local_name: "nope",
                        evidence: Some("import:./nope.js"),
                        ..Spec::default()
                    },
                ),
                raw(
                    "imports",
                    REL_B_MODULE,
                    "../../../outside",
                    7,
                    Spec {
                        local_name: "outside",
                        evidence: Some("import:../../../outside"),
                        ..Spec::default()
                    },
                ),
                call(REL_B_RUN, "helper", 9, None, "call:helper"),
            ],
            vec![decl("function", "src/a/b.ts", "", "run", 8)],
        ),
        source_unit(
            "src/a/x.ts",
            Vec::new(),
            vec![decl("function", "src/a/x.ts", "", "helper", 2)],
        ),
        source_unit("src/a/dir/index.ts", Vec::new(), Vec::new()),
        source_unit("src/a/thing.ts", Vec::new(), Vec::new()),
        source_unit("src/other/thing.ts", Vec::new(), Vec::new()),
        source_unit("src/other/b.ts", Vec::new(), Vec::new()),
        source_unit("src/other/x.ts", Vec::new(), Vec::new()),
        source_unit("src/x.ts", Vec::new(), Vec::new()),
        source_unit("src/outside.ts", Vec::new(), Vec::new()),
    ]
}

static RELATIVE: LazyLock<Fixture> = LazyLock::new(|| Fixture::new(relative_units()));

fn relative_import_at(line: u32) -> Option<&'static str> {
    RELATIVE
        .result
        .edges
        .iter()
        .find(|edge| edge.file == "src/a/b.ts" && edge.kind == "imports" && edge.line == line)
        .map(|edge| edge.dst.as_str())
}

fn relative_failure_at(line: u32) -> Option<&'static UnresolvedBinding> {
    RELATIVE
        .result
        .unresolved
        .iter()
        .find(|entry| entry.file == "src/a/b.ts" && entry.line == line)
}

#[test]
fn the_importer_is_what_makes_a_relative_specifier_mean_a_unit() {
    let index = &RELATIVE.index;
    assert_eq!(
        index.module_candidates("./x.js", Some("src/a/b.ts")),
        vec!["src/a/x.ts".to_owned()]
    );
    assert_eq!(
        index.module_file("./x.js", Some("src/a/b.ts")).as_deref(),
        Some("src/a/x.ts")
    );
    // `x` alone ties between `src/a/x.ts` and `src/other/x.ts`, so the module-string reading cannot
    // answer.
    assert_eq!(index.module_file("./x.js", None), None);
}

#[test]
fn relative_1_dot_slash_x_binds_x_for_the_import_and_for_the_call_it_feeds() {
    assert_eq!(relative_import_at(2), Some(REL_AX_FILE));
    assert!(
        RELATIVE
            .result
            .edges
            .iter()
            .any(|edge| edge.kind == "imports"
                && edge.file == "src/a/b.ts"
                && edge.line == 2
                && edge.src == REL_B_MODULE
                && edge.dst == REL_AX_FILE
                && edge.tier == "extracted")
    );
    // Step 2 of the call order reads the same import table, so the name it made visible has to
    // resolve through the importer-relative rule too — `inferred` would mean it fell through to the
    // unique-name tier instead.
    let call = RELATIVE
        .result
        .edges
        .iter()
        .find(|edge| edge.kind == "calls" && edge.line == 9)
        .expect("the call at line 9 binds");
    assert_eq!(
        (call.src.as_str(), call.dst.as_str(), call.tier.as_str()),
        (REL_B_RUN, REL_AX_HELPER, "extracted")
    );
}

#[test]
fn relative_2_dot_slash_dir_index_lands_in_the_directorys_index_unit() {
    assert_eq!(relative_import_at(3), Some(REL_DIR_FILE));
    assert_eq!(
        RELATIVE
            .index
            .module_file("./dir/index.js", Some("src/a/b.ts"))
            .as_deref(),
        Some("src/a/dir/index.ts")
    );
}

#[test]
fn relative_3_the_importers_own_directory_beats_a_duplicate_basename() {
    assert_eq!(
        RELATIVE.index.module_candidates("thing", None),
        vec!["src/a/thing.ts".to_owned(), "src/other/thing.ts".to_owned()]
    );
    assert_eq!(RELATIVE.index.module_file("thing", None), None);

    // `./thing.js` in `src/a/` names the neighbour, so this binds instead of reporting the tie.
    assert_eq!(relative_import_at(4), Some(REL_THING_FILE));
    assert!(relative_failure_at(4).is_none());
}

#[test]
fn relative_4_dot_dot_climbs_out_of_the_importers_directory_and_still_binds() {
    assert_eq!(relative_import_at(5), Some(REL_X_FILE));
    assert_eq!(
        RELATIVE
            .index
            .module_file("../x.js", Some("src/a/b.ts"))
            .as_deref(),
        Some("src/x.ts")
    );

    // The memo is keyed by specifier *and* importer: the same `./x.js` names a different unit for an
    // importer in another directory, so the two cannot share an entry.
    assert_eq!(
        RELATIVE
            .index
            .module_file("./x.js", Some("src/other/b.ts"))
            .as_deref(),
        Some("src/other/x.ts")
    );
    assert_eq!(
        RELATIVE
            .index
            .module_file("./x.js", Some("src/a/b.ts"))
            .as_deref(),
        Some("src/a/x.ts")
    );
}

#[test]
fn relative_5_a_relative_target_that_is_not_in_the_tree_stays_missing_module() {
    assert!(relative_import_at(6).is_none());
    assert_eq!(
        relative_failure_at(6),
        Some(&UnresolvedBinding {
            file: "src/a/b.ts".to_owned(),
            line: 6,
            raw_target: "./nope.js".to_owned(),
            evidence_kind: "missing_module".to_owned(),
            evidence: "import:./nope.js (no file matched)".to_owned(),
        })
    );
}

#[test]
fn relative_6_a_specifier_that_climbs_above_the_root_does_not_bind() {
    // `src/outside.ts` is the unit that name would reach, and it must still not be chosen: the
    // specifier leaves the tree, so it falls back to the module-string reading and matches nothing.
    assert_eq!(
        RELATIVE.index.module_candidates("outside", None),
        vec!["src/outside.ts".to_owned()]
    );
    assert!(
        RELATIVE
            .index
            .module_candidates("../../../outside", Some("src/a/b.ts"))
            .is_empty()
    );
    assert!(relative_import_at(7).is_none());
    let entry = relative_failure_at(7).expect("line 7 is recorded");
    assert_eq!(
        (entry.raw_target.as_str(), entry.evidence_kind.as_str()),
        ("../../../outside", "missing_module")
    );
}
