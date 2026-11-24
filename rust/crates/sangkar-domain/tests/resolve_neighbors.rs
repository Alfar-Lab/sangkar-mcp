//! Resolution and expansion against hand-written, reader-shaped fakes over in-memory fixtures.
//!
//! Ported from `tests/domain.test.ts`. No database and no `GraphReader` import: each fake has
//! exactly the members the module under test calls, so a widened dependency would stop compiling
//! here, and every expectation is the TypeScript expectation rather than a new one.

use std::collections::HashMap;

use sangkar_core::errors::AppError;
use sangkar_domain::neighbors::{
    Direction, EXPAND_DEPTH_MAX, GroupedNeighbors, NeighbourSource, clamp_depth, expand,
};
use sangkar_domain::resolve::{
    CANDIDATE_CAP, RESOLUTION_STEPS, Resolution, ResolutionCandidate, ResolutionSource,
    ResolutionStep, ResolveOptions, resolve,
};
use sangkar_store::read::edges::{EdgeRow, NeighbourEdge};
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::LineRange;

// -------------------------------------------------------------------------------------------
// fixtures: a `NodeRow` with the columns resolution and expansion actually read made explicit
// -------------------------------------------------------------------------------------------

fn node_of(id: i64, name: &str, qn: &str, path: &str) -> NodeRow {
    NodeRow {
        id,
        file_id: id,
        path: path.to_owned(),
        parent_id: None,
        kind: "function".to_owned(),
        subkind: None,
        name: name.to_owned(),
        qn: qn.to_owned(),
        start_line: 1,
        end_line: 1,
        signature: None,
        docstring: None,
        is_test: false,
        props: "{}".to_owned(),
    }
}

fn with_kind(mut node: NodeRow, kind: &str) -> NodeRow {
    node.kind = kind.to_owned();
    node
}

fn with_lines(mut node: NodeRow, start_line: i64, end_line: i64) -> NodeRow {
    node.start_line = start_line;
    node.end_line = end_line;
    node
}

fn with_parent(mut node: NodeRow, parent_id: i64) -> NodeRow {
    node.parent_id = Some(parent_id);
    node
}

fn with_props(mut node: NodeRow, props: &str) -> NodeRow {
    node.props = props.to_owned();
    node
}

// -------------------------------------------------------------------------------------------
// the resolution fake: the store's two lookups over a fixed list, `findByName` ordered by `qn`
// -------------------------------------------------------------------------------------------

/// The store's `findByName`: an all-or-nothing kinds filter, ordered by `qn`.
fn by_name(nodes: &[NodeRow], name: &str, kinds: Option<&[String]>) -> Vec<NodeRow> {
    let filter = kinds.unwrap_or_default();
    let mut matching: Vec<NodeRow> = nodes
        .iter()
        .filter(|node| node.name == name)
        .filter(|node| filter.is_empty() || filter.contains(&node.kind))
        .cloned()
        .collect();
    matching.sort_by(|left, right| left.qn.cmp(&right.qn));
    matching
}

fn by_qn(nodes: &[NodeRow], qn: &str) -> Option<NodeRow> {
    nodes.iter().find(|node| node.qn == qn).cloned()
}

/// A source with no decorator query, so the trait's own fallback answers step 5.
struct FakeSource {
    nodes: Vec<NodeRow>,
}

impl ResolutionSource for FakeSource {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        Ok(by_qn(&self.nodes, qn))
    }

    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError> {
        Ok(by_name(&self.nodes, name, kinds))
    }
}

/// The same list behind a source with a props index, which is what the decorator step prefers.
struct DecoratedSource {
    nodes: Vec<NodeRow>,
}

impl ResolutionSource for DecoratedSource {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        Ok(by_qn(&self.nodes, qn))
    }

    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError> {
        Ok(by_name(&self.nodes, name, kinds))
    }

    fn find_by_decorator(
        &self,
        name: &str,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NodeRow>, AppError> {
        if name != "Injectable" {
            return Ok(Vec::new());
        }
        Ok(by_name(&self.nodes, "Beta", kinds))
    }
}

fn source(nodes: &[NodeRow]) -> FakeSource {
    FakeSource {
        nodes: nodes.to_vec(),
    }
}

fn decorated(nodes: &[NodeRow]) -> DecoratedSource {
    DecoratedSource {
        nodes: nodes.to_vec(),
    }
}

fn options() -> ResolveOptions {
    ResolveOptions::default()
}

fn kinds(kinds: &[&str]) -> ResolveOptions {
    ResolveOptions {
        kinds: Some(kinds.iter().map(|kind| (*kind).to_owned()).collect()),
        path: None,
    }
}

fn under(path: &str) -> ResolveOptions {
    ResolveOptions {
        kinds: None,
        path: Some(path.to_owned()),
    }
}

fn ok_result(resolution: Resolution) -> (ResolutionStep, NodeRow, f64, String) {
    let Resolution::Ok {
        step,
        node,
        score,
        why,
    } = resolution
    else {
        panic!("expected ok, got {resolution:?}");
    };
    (step, node, score, why)
}

fn ambiguous_result(
    resolution: Resolution,
) -> (ResolutionStep, Vec<ResolutionCandidate>, usize, usize) {
    let Resolution::Ambiguous {
        step,
        candidates,
        candidates_total,
        candidates_shown,
    } = resolution
    else {
        panic!("expected ambiguous, got {resolution:?}");
    };
    (step, candidates, candidates_total, candidates_shown)
}

fn not_found_result(resolution: Resolution) -> Vec<ResolutionCandidate> {
    let Resolution::NotFound { near_misses } = resolution else {
        panic!("expected not_found, got {resolution:?}");
    };
    near_misses
}

fn one(resolution: Result<Resolution, AppError>) -> Resolution {
    resolution.expect("resolve")
}

fn resolve_names(resolution: Resolution) -> Vec<String> {
    not_found_result(resolution)
        .into_iter()
        .map(|candidate| candidate.qn)
        .collect()
}

// -------------------------------------------------------------------------------------------
// resolve: the detection order; one file node, one decorated class, two `alpha`s in two files,
// and one node whose *name* is another node's `qn` so two steps can compete
// -------------------------------------------------------------------------------------------

fn file_node() -> NodeRow {
    with_lines(
        with_kind(node_of(1, "a.ts", "src/a.ts::a.ts", "src/a.ts"), "file"),
        1,
        40,
    )
}

fn beta() -> NodeRow {
    with_props(
        with_kind(node_of(2, "Beta", "src/a.ts::Beta", "src/a.ts"), "class"),
        r#"{"decorators":["Injectable"]}"#,
    )
}

fn alpha_method() -> NodeRow {
    with_lines(
        with_parent(
            with_kind(
                node_of(3, "alpha", "src/a.ts::Beta.alpha", "src/a.ts"),
                "method",
            ),
            2,
        ),
        10,
        20,
    )
}

fn alpha_function() -> NodeRow {
    with_lines(node_of(4, "alpha", "src/b.ts::alpha", "src/b.ts"), 3, 9)
}

fn zeta() -> NodeRow {
    node_of(5, "zeta", "src/b.ts::zeta", "src/b.ts")
}

/// Named `a`, so the path step sees two candidates inside `src/a.ts`.
fn short_a() -> NodeRow {
    node_of(6, "a", "src/a.ts::a", "src/a.ts")
}

/// Named after another node's `qn`: only a deliberate fixture can make two steps compete.
fn qn_named() -> NodeRow {
    node_of(7, "src/a.ts::Beta.alpha", "src/b.ts::weird", "src/b.ts")
}

/// Named `ts`, so `src/a.ts` also has a dotted tail to find.
fn ts_named() -> NodeRow {
    node_of(8, "ts", "src/b.ts::ts", "src/b.ts")
}

fn resolve_nodes() -> Vec<NodeRow> {
    vec![
        file_node(),
        beta(),
        alpha_method(),
        alpha_function(),
        zeta(),
        short_a(),
        qn_named(),
    ]
}

#[test]
fn the_step_table_is_the_contract_order() {
    assert_eq!(
        RESOLUTION_STEPS,
        ["exact_qn", "exact_name", "dotted_tail", "path", "decorator"]
    );
    for (index, step) in [
        ResolutionStep::ExactQn,
        ResolutionStep::ExactName,
        ResolutionStep::DottedTail,
        ResolutionStep::Path,
        ResolutionStep::Decorator,
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(step.as_str(), RESOLUTION_STEPS[index]);
    }
}

#[test]
fn step_one_exact_qn_resolves_without_consulting_a_name() {
    let (step, node, score, why) = ok_result(one(resolve(
        &source(&resolve_nodes()),
        "src/a.ts::Beta.alpha",
        &options(),
    )));
    assert_eq!(step, ResolutionStep::ExactQn);
    assert_eq!(node.id, 3);
    assert_eq!(score, 1.0);
    assert_eq!(why, "exact qualified name");
}

#[test]
fn step_two_exact_name_resolves_a_bare_name() {
    let (step, node, _, why) =
        ok_result(one(resolve(&source(&resolve_nodes()), "zeta", &options())));
    assert_eq!(step, ResolutionStep::ExactName);
    assert_eq!(node.id, 5);
    assert_eq!(why, "exact name, 1 of 1 candidate");
}

#[test]
fn step_three_dotted_tail_prefers_the_tail_inside_its_own_scope() {
    let (step, node, score, why) = ok_result(one(resolve(
        &source(&resolve_nodes()),
        "Beta.alpha",
        &options(),
    )));
    assert_eq!(step, ResolutionStep::DottedTail);
    assert_eq!(node.id, 3);
    assert_eq!(score, 0.85);
    assert!(why.contains("own scope"), "{why}");
    assert_eq!(
        why,
        "dotted name \"Beta.alpha\" matched in the node's own scope"
    );
}

#[test]
fn step_four_path_resolves_to_the_file_node_ahead_of_a_symbol_in_that_file() {
    let (step, node, score, why) = ok_result(one(resolve(
        &source(&resolve_nodes()),
        "src/a.ts",
        &options(),
    )));
    assert_eq!(step, ResolutionStep::Path);
    assert_eq!(node.kind, "file");
    assert_eq!(node.id, 1);
    assert_eq!(score, 0.75);
    assert_eq!(why, "file \"src/a.ts\"");
}

#[test]
fn step_five_decorator_is_a_property_filter_never_a_node_kind() {
    let nodes = resolve_nodes();
    let (step, node, _, why) =
        ok_result(one(resolve(&decorated(&nodes), "@Injectable", &options())));
    assert_eq!(step, ResolutionStep::Decorator);
    assert_eq!(node.id, 2);
    // The answer is the decorated class, not a synthesised `decorator` node.
    assert_eq!(node.kind, "class");
    assert_eq!(why, "property filter @Injectable on \"Beta\"");
}

#[test]
fn the_decorator_step_stays_a_filter_when_kinds_exclude_the_decorated_node() {
    let nodes = resolve_nodes();
    assert!(matches!(
        one(resolve(
            &decorated(&nodes),
            "@Injectable",
            &kinds(&["function"])
        )),
        Resolution::NotFound { .. }
    ));
    let (_, node, _, _) = ok_result(one(resolve(
        &decorated(&nodes),
        "@Injectable",
        &kinds(&["class"]),
    )));
    assert_eq!(node.id, 2);
}

#[test]
fn without_a_props_query_the_decorator_step_keeps_only_matching_names() {
    // No node is named `Injectable`, so the fallback cannot invent one.
    assert!(matches!(
        one(resolve(
            &source(&resolve_nodes()),
            "@Injectable",
            &options()
        )),
        Resolution::NotFound { .. }
    ));
}

#[test]
fn the_decorator_fallback_still_answers_when_the_name_matches_and_the_property_is_carried() {
    let nodes = vec![with_props(
        with_kind(
            node_of(9, "Injectable", "src/i.ts::Injectable", "src/i.ts"),
            "class",
        ),
        r#"{"decorators":["@Injectable()"]}"#,
    )];
    let (step, node, _, why) = ok_result(one(resolve(&source(&nodes), "@Injectable", &options())));
    assert_eq!(step, ResolutionStep::Decorator);
    assert_eq!(node.id, 9);
    assert_eq!(why, "property filter @Injectable on \"Injectable\"");
}

#[test]
fn precedence_exact_qn_wins_over_an_exact_name_that_also_matches() {
    // `qn_named`'s name is exactly `alpha_method`'s qn, so steps 1 and 2 both have an answer.
    let (step, node, _, _) = ok_result(one(resolve(
        &source(&resolve_nodes()),
        "src/a.ts::Beta.alpha",
        &options(),
    )));
    assert_eq!(step, ResolutionStep::ExactQn);
    assert_eq!(node.id, 3);
}

#[test]
fn precedence_exact_name_wins_over_a_dotted_tail() {
    let mut nodes = resolve_nodes();
    nodes.push(node_of(
        20,
        "Beta.alpha",
        "src/c.ts::Beta.alpha",
        "src/c.ts",
    ));
    let (step, node, _, _) = ok_result(one(resolve(&source(&nodes), "Beta.alpha", &options())));
    assert_eq!(step, ResolutionStep::ExactName);
    assert_eq!(node.id, 20);
}

#[test]
fn precedence_a_dotted_tail_wins_over_a_path_when_both_could_answer() {
    let mut nodes = resolve_nodes();
    nodes.push(ts_named());
    let (step, node, _, _) = ok_result(one(resolve(&source(&nodes), "src/a.ts", &options())));
    assert_eq!(step, ResolutionStep::DottedTail);
    assert_eq!(node.id, 8);
    // The same query with no `ts` node: the path step answers, proving the order and not a special
    // case.
    let (step, _, _, _) = ok_result(one(resolve(
        &source(&resolve_nodes()),
        "src/a.ts",
        &options(),
    )));
    assert_eq!(step, ResolutionStep::Path);
}

#[test]
fn a_dotted_query_with_no_tail_cannot_match() {
    assert!(matches!(
        one(resolve(&source(&resolve_nodes()), "Beta.", &options())),
        Resolution::NotFound { .. }
    ));
}

// -------------------------------------------------------------------------------------------
// resolve: ambiguity
// -------------------------------------------------------------------------------------------

#[test]
fn two_candidates_at_the_same_score_are_ambiguous_with_both() {
    let (step, candidates, candidates_total, candidates_shown) =
        ambiguous_result(one(resolve(&source(&resolve_nodes()), "alpha", &options())));
    assert_eq!(step, ResolutionStep::ExactName);
    assert_eq!(candidates.len(), 2);
    // Nothing was cut, so the tie size and the shown size are both two.
    assert_eq!(candidates_total, 2);
    assert_eq!(candidates_shown, 2);

    let first = &candidates[0];
    let second = &candidates[1];
    assert_eq!(first.score, second.score);
    assert_eq!(
        [first.qn.as_str(), second.qn.as_str()],
        ["src/a.ts::Beta.alpha", "src/b.ts::alpha"]
    );
    assert_eq!(first.kind, "method");
    assert_eq!(second.kind, "function");
    assert_eq!(first.file, "src/a.ts");
    assert_eq!(first.lines, LineRange { start: 10, end: 20 });
    assert_eq!(first.why, "exact name, 1 of 2 candidates");
    assert_eq!(second.why, "exact name, 2 of 2 candidates");
}

#[test]
fn kind_narrowing_turns_an_ambiguous_name_into_an_answer() {
    let (step, node, _, why) = ok_result(one(resolve(
        &source(&resolve_nodes()),
        "alpha",
        &kinds(&["function"]),
    )));
    assert_eq!(step, ResolutionStep::ExactName);
    assert_eq!(node.id, 4);
    assert_eq!(why, "exact name, 1 of 1 candidate");
}

#[test]
fn a_tie_wider_than_the_contract_cap_is_cut_to_five_candidates() {
    let many: Vec<NodeRow> = (0..7)
        .map(|index| {
            node_of(
                100 + index,
                "dup",
                &format!("src/f{index}.ts::dup"),
                &format!("src/f{index}.ts"),
            )
        })
        .collect();
    let (_, candidates, candidates_total, candidates_shown) =
        ambiguous_result(one(resolve(&source(&many), "dup", &options())));
    assert_eq!(candidates.len(), CANDIDATE_CAP);
    let unique: std::collections::BTreeSet<&String> =
        candidates.iter().map(|candidate| &candidate.qn).collect();
    assert_eq!(unique.len(), CANDIDATE_CAP);
    // The cap hides rows, never the size of the tie: the totals come from the winners before the
    // slice, and every `why` is built from that same number, so phrase and field cannot disagree.
    assert_eq!(candidates_total, 7);
    assert_eq!(candidates_shown, candidates.len());
    assert_eq!(candidates_shown, CANDIDATE_CAP);
    assert!(candidates_total > candidates_shown);
    assert_eq!(candidates[0].why, "exact name, 1 of 7 candidates");
    assert_eq!(candidates[4].why, "exact name, 5 of 7 candidates");
    for candidate in &candidates {
        assert!(
            candidate
                .why
                .contains(&format!("of {candidates_total} candidates")),
            "{}",
            candidate.why
        );
    }
}

// -------------------------------------------------------------------------------------------
// resolve: the path prefix filter
// -------------------------------------------------------------------------------------------

fn dup_nodes() -> Vec<NodeRow> {
    vec![
        node_of(40, "dup", "src/a.ts::dup", "src/a.ts"),
        node_of(41, "dup", "src/nested/b.ts::dup", "src/nested/b.ts"),
        node_of(42, "dup", "lib/c.ts::dup", "lib/c.ts"),
    ]
}

#[test]
fn path_narrows_three_same_name_candidates_to_one() {
    let nodes = dup_nodes();
    let (_, _, total, _) = ambiguous_result(one(resolve(&source(&nodes), "dup", &options())));
    assert_eq!(total, 3);
    let (step, node, _, why) =
        ok_result(one(resolve(&source(&nodes), "dup", &under("src/nested"))));
    assert_eq!(step, ResolutionStep::ExactName);
    assert_eq!(node.id, 41);
    // The survivor counts the filtered total, not the unfiltered one.
    assert_eq!(why, "exact name, 1 of 1 candidate");
}

#[test]
fn path_keeps_a_tie_that_is_still_a_tie_inside_the_prefix() {
    let (_, candidates, total, _) =
        ambiguous_result(one(resolve(&source(&dup_nodes()), "dup", &under("src"))));
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.qn.as_str())
            .collect::<Vec<_>>(),
        ["src/a.ts::dup", "src/nested/b.ts::dup"]
    );
    assert_eq!(total, 2);
}

#[test]
fn a_path_prefix_does_not_match_a_sibling_whose_name_starts_with_it() {
    // `src-old` sorts below `src/`, which is why the half-open bounds exist: a prefix must not
    // answer from a sibling directory.
    let nodes = vec![
        node_of(50, "dup", "src-old/x.ts::dup", "src-old/x.ts"),
        node_of(51, "dup", "src/x.ts::dup", "src/x.ts"),
    ];
    let (_, _, total, _) = ambiguous_result(one(resolve(&source(&nodes), "dup", &options())));
    assert_eq!(total, 2);
    let (_, node, _, why) = ok_result(one(resolve(&source(&nodes), "dup", &under("src"))));
    assert_eq!(node.id, 51);
    assert_eq!(why, "exact name, 1 of 1 candidate");
}

#[test]
fn path_that_excludes_every_candidate_is_not_found_with_only_filtered_near_misses() {
    // `dup.nope` is no step's match, and unfiltered its miss lists all three `dup` symbols.
    assert_eq!(
        not_found_result(one(resolve(&source(&dup_nodes()), "dup.nope", &options()))).len(),
        3
    );
    assert_eq!(
        not_found_result(one(resolve(
            &source(&dup_nodes()),
            "dup.nope",
            &under("nowhere")
        ))),
        Vec::new()
    );
    // A prefix that holds one of them keeps exactly that one, inside it.
    assert_eq!(
        resolve_names(one(resolve(
            &source(&dup_nodes()),
            "dup.nope",
            &under("lib")
        ))),
        ["lib/c.ts::dup"]
    );
}

#[test]
fn path_filters_the_dotted_tail_path_and_decorator_steps_too() {
    let nodes = resolve_nodes();
    assert_eq!(
        ok_result(one(resolve(&source(&nodes), "Beta.alpha", &under("src"))))
            .1
            .id,
        3
    );
    assert!(matches!(
        one(resolve(&source(&nodes), "Beta.alpha", &under("lib"))),
        Resolution::NotFound { .. }
    ));

    assert_eq!(
        ok_result(one(resolve(&source(&nodes), "src/a.ts", &under("src")))).0,
        ResolutionStep::Path
    );
    assert!(matches!(
        one(resolve(&source(&nodes), "src/a.ts", &under("lib"))),
        Resolution::NotFound { .. }
    ));

    assert_eq!(
        ok_result(one(resolve(
            &decorated(&nodes),
            "@Injectable",
            &under("src/a.ts")
        )))
        .1
        .id,
        2
    );
    assert!(matches!(
        one(resolve(&decorated(&nodes), "@Injectable", &under("lib"))),
        Resolution::NotFound { .. }
    ));
}

#[test]
fn a_filtered_exact_qn_never_answers_from_outside_the_prefix() {
    let nodes = resolve_nodes();
    assert_eq!(
        ok_result(one(resolve(
            &source(&nodes),
            "src/b.ts::zeta",
            &under("src/b.ts")
        )))
        .1
        .id,
        5
    );
    assert!(matches!(
        one(resolve(
            &source(&nodes),
            "src/b.ts::zeta",
            &under("src/a.ts")
        )),
        Resolution::NotFound { .. }
    ));
}

#[test]
fn no_path_an_empty_path_and_an_absent_option_answer_as_before() {
    let nodes = resolve_nodes();
    let plain = ambiguous_result(one(resolve(&source(&nodes), "alpha", &options()))).1;
    assert_eq!(
        ambiguous_result(one(resolve(
            &source(&nodes),
            "alpha",
            &ResolveOptions::default()
        )))
        .1,
        plain
    );
    assert_eq!(
        ambiguous_result(one(resolve(&source(&nodes), "alpha", &under("")))).1,
        plain
    );
    assert_eq!(
        ambiguous_result(one(resolve(&source(&nodes), "alpha", &under("   ")))).1,
        plain
    );
    // `kinds` still narrows the way the pre-options call shape did.
    assert_eq!(
        ok_result(one(resolve(
            &source(&nodes),
            "alpha",
            &kinds(&["function"])
        )))
        .1
        .id,
        4
    );
}

// -------------------------------------------------------------------------------------------
// resolve: not_found
// -------------------------------------------------------------------------------------------

#[test]
fn not_found_carries_near_misses_instead_of_a_bare_zero() {
    let near_misses = not_found_result(one(resolve(
        &source(&resolve_nodes()),
        "Beta.missing",
        &options(),
    )));
    assert_eq!(near_misses.len(), 1);
    let near = &near_misses[0];
    assert_eq!(near.qn, "src/a.ts::Beta");
    assert!(near.why.contains("segment \"Beta\""), "{}", near.why);
    assert!(near.score < 0.6);
    assert_eq!(
        near.why,
        "segment \"Beta\" of \"Beta.missing\" matched a symbol"
    );
}

#[test]
fn an_empty_query_is_not_found_with_no_near_misses() {
    assert_eq!(
        not_found_result(one(resolve(&source(&resolve_nodes()), "   ", &options()))),
        Vec::new()
    );
}

#[test]
fn a_path_query_with_no_match_still_reports_segment_near_misses() {
    let near_misses = not_found_result(one(resolve(
        &source(&resolve_nodes()),
        "src/a.ts::nope",
        &options(),
    )));
    assert!(near_misses.len() <= CANDIDATE_CAP);
}

// -------------------------------------------------------------------------------------------
// expand: grouping by edge kind and tier, hop and external flags
// -------------------------------------------------------------------------------------------

fn edge_of(id: i64, src_id: i64, dst_id: i64, kind: &str, tier: &str, line: i64) -> EdgeRow {
    EdgeRow {
        id,
        src_id,
        dst_id,
        kind: kind.to_owned(),
        tier: tier.to_owned(),
        file_id: 1,
        line,
        local_name: String::new(),
        evidence: None,
    }
}

fn link(edge: EdgeRow, node: NodeRow) -> NeighbourEdge {
    NeighbourEdge { edge, node }
}

/// The fixture graph, keyed by the near side of the edge.
struct FakeGraph {
    inbound: HashMap<i64, Vec<NeighbourEdge>>,
    outbound: HashMap<i64, Vec<NeighbourEdge>>,
}

fn filtered(edges: Option<&Vec<NeighbourEdge>>, kinds: Option<&[String]>) -> Vec<NeighbourEdge> {
    let list = edges.map(Vec::as_slice).unwrap_or_default();
    let filter = kinds.unwrap_or_default();
    list.iter()
        .filter(|edge| filter.is_empty() || filter.contains(&edge.edge.kind))
        .cloned()
        .collect()
}

impl NeighbourSource for FakeGraph {
    fn inbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        Ok(filtered(self.inbound.get(&node_id), kinds))
    }

    fn outbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        Ok(filtered(self.outbound.get(&node_id), kinds))
    }
}

fn seed() -> NodeRow {
    node_of(1, "seed", "src/seed.ts::seed", "src/seed.ts")
}

fn caller() -> NodeRow {
    with_lines(
        node_of(2, "caller", "src/seed.ts::caller", "src/seed.ts"),
        7,
        7,
    )
}

fn builtin() -> NodeRow {
    with_props(
        with_kind(node_of(3, "dict", "builtins.dict", ""), "module"),
        r#"{"external":true}"#,
    )
}

fn untagged_external() -> NodeRow {
    with_kind(node_of(7, "abc", "collections.abc", ""), "module")
}

fn deep() -> NodeRow {
    with_lines(node_of(4, "deep", "src/deep.ts::deep", "src/deep.ts"), 2, 2)
}

fn inbound_one() -> NodeRow {
    with_lines(node_of(5, "user", "src/a.ts::user", "src/a.ts"), 4, 4)
}

fn inbound_two() -> NodeRow {
    with_lines(
        with_kind(node_of(6, "Beta", "src/a.ts::Beta", "src/a.ts"), "class"),
        9,
        9,
    )
}

/// `seed --calls--> caller --calls--> deep --calls--> seed` (a cycle back to the seed).
fn expand_graph() -> FakeGraph {
    FakeGraph {
        outbound: HashMap::from([
            (
                1,
                vec![
                    link(edge_of(1, 1, 2, "calls", "extracted", 10), caller()),
                    link(edge_of(2, 1, 3, "references", "inferred", 12), builtin()),
                    link(
                        edge_of(3, 1, 7, "references", "inferred", 13),
                        untagged_external(),
                    ),
                ],
            ),
            (
                2,
                vec![link(edge_of(4, 2, 4, "calls", "extracted", 20), deep())],
            ),
            (
                4,
                vec![link(edge_of(5, 4, 1, "calls", "extracted", 30), seed())],
            ),
        ]),
        inbound: HashMap::from([
            (
                1,
                vec![link(
                    edge_of(6, 5, 1, "calls", "extracted", 40),
                    inbound_one(),
                )],
            ),
            (
                5,
                vec![link(
                    edge_of(7, 6, 5, "contains", "extracted", 50),
                    inbound_two(),
                )],
            ),
        ]),
    }
}

fn expand_graph_at(direction: Direction, depth: u8, kinds: Option<&[String]>) -> GroupedNeighbors {
    expand(&expand_graph(), 1, direction, depth, kinds).expect("expand")
}

#[test]
fn groups_are_keyed_by_edge_kind_then_tier_in_both_directions() {
    let result = expand_graph_at(Direction::All, 2, None);
    assert_eq!(result.depth, 2);
    assert_eq!(result.seeds, vec![1]);
    assert_eq!(
        result
            .outbound
            .iter()
            .map(|group| format!("{}/{}", group.kind, group.tier))
            .collect::<Vec<_>>(),
        ["calls/extracted", "references/inferred"]
    );
    assert_eq!(
        result
            .inbound
            .iter()
            .map(|group| format!("{}/{}", group.kind, group.tier))
            .collect::<Vec<_>>(),
        ["calls/extracted", "contains/extracted"]
    );
}

#[test]
fn items_carry_hop_external_and_the_readable_location() {
    let result = expand_graph_at(Direction::All, 2, None);
    let calls = &result.outbound[0];
    assert_eq!(
        calls
            .items
            .iter()
            .map(|item| (
                item.id,
                item.qn.as_str(),
                item.kind.as_str(),
                item.file.as_str(),
                item.line,
                item.hop,
                item.external
            ))
            .collect::<Vec<_>>(),
        [
            (
                2,
                "src/seed.ts::caller",
                "function",
                "src/seed.ts",
                7,
                1,
                false
            ),
            (
                4,
                "src/deep.ts::deep",
                "function",
                "src/deep.ts",
                2,
                2,
                false
            ),
        ]
    );

    let references = &result.outbound[1];
    assert_eq!(
        references
            .items
            .iter()
            .map(|item| (item.qn.as_str(), item.hop, item.external))
            .collect::<Vec<_>>(),
        [("builtins.dict", 1, true), ("collections.abc", 1, true)]
    );

    assert_eq!(
        result.inbound[0]
            .items
            .iter()
            .map(|item| (item.id, item.hop))
            .collect::<Vec<_>>(),
        [(5, 1)]
    );
    assert_eq!(
        result.inbound[1]
            .items
            .iter()
            .map(|item| (item.id, item.hop))
            .collect::<Vec<_>>(),
        [(6, 2)]
    );
}

#[test]
fn a_cycle_back_to_a_seed_is_not_reported_as_a_neighbour() {
    let result = expand_graph_at(Direction::All, 4, None);
    let ids: Vec<i64> = result
        .outbound
        .iter()
        .chain(result.inbound.iter())
        .flat_map(|group| group.items.iter().map(|item| item.id))
        .collect();
    assert!(!ids.contains(&1));
}

#[test]
fn depth_clamps_to_one_through_four_and_is_reported_as_walked() {
    assert_eq!(clamp_depth(9), EXPAND_DEPTH_MAX);
    assert_eq!(clamp_depth(0), 1);
    assert_eq!(clamp_depth(-3), 1);
    assert_eq!(clamp_depth(2), 2);
    // The walk clamps a `u8` budget too, so a caller that skipped `clamp_depth` still cannot widen
    // it past the ceiling or below the smallest useful walk.
    assert_eq!(
        expand_graph_at(Direction::All, 9, None).depth,
        EXPAND_DEPTH_MAX
    );
    assert_eq!(expand_graph_at(Direction::All, 0, None).depth, 1);
}

fn chain(edges: &[(i64, i64)], nodes: &[NodeRow]) -> FakeGraph {
    let mut outbound: HashMap<i64, Vec<NeighbourEdge>> = HashMap::new();
    let mut inbound: HashMap<i64, Vec<NeighbourEdge>> = HashMap::new();
    for (index, (src_id, dst_id)) in edges.iter().enumerate() {
        let target = nodes
            .iter()
            .find(|node| node.id == *dst_id)
            .expect("the chain names an existing node");
        let edge_id = i64::try_from(index).expect("a small chain") + 1;
        outbound.entry(*src_id).or_default().push(link(
            edge_of(edge_id, *src_id, *dst_id, "calls", "extracted", edge_id),
            target.clone(),
        ));
        let source_node = nodes
            .iter()
            .find(|node| node.id == *src_id)
            .expect("the chain names an existing node");
        inbound.entry(*dst_id).or_default().push(link(
            edge_of(edge_id, *src_id, *dst_id, "calls", "extracted", edge_id),
            source_node.clone(),
        ));
    }
    FakeGraph { inbound, outbound }
}

#[test]
fn depth_one_sees_only_the_first_hop() {
    let result = expand_graph_at(Direction::Out, 1, None);
    let ids: Vec<i64> = result
        .outbound
        .iter()
        .flat_map(|group| group.items.iter().map(|item| item.id))
        .collect();
    assert_eq!(ids, [2, 3, 7]);
    assert!(!ids.contains(&4));
}

#[test]
fn depth_three_walks_to_the_third_hop_in_both_directions() {
    // four symbols in a line: 1 -> 2 -> 3 -> 4, so the third hop is the only way to reach node 4.
    let nodes: Vec<NodeRow> = (1..=4)
        .map(|id| {
            node_of(
                id,
                &format!("n{id}"),
                &format!("src/n{id}.ts::n{id}"),
                &format!("src/n{id}.ts"),
            )
        })
        .collect();
    let graph = chain(&[(1, 2), (2, 3), (3, 4)], &nodes);

    let outbound = expand(&graph, 1, Direction::All, 3, None).expect("expand");
    assert_eq!(outbound.depth, 3);
    assert_eq!(
        outbound.outbound[0]
            .items
            .iter()
            .map(|item| (item.id, item.hop))
            .collect::<Vec<_>>(),
        [(2, 1), (3, 2), (4, 3)]
    );
    // Two hops stop before the fourth symbol; the hop is what the walk budget buys.
    let shorter = expand(&graph, 1, Direction::All, 2, None).expect("expand");
    assert_eq!(
        shorter.outbound[0]
            .items
            .iter()
            .map(|item| (item.id, item.hop))
            .collect::<Vec<_>>(),
        [(2, 1), (3, 2)]
    );

    // The inbound half is the same line walked backwards from its far end.
    let inbound = expand(&graph, 4, Direction::All, 3, None).expect("expand");
    assert_eq!(inbound.depth, 3);
    assert!(inbound.outbound.is_empty());
    assert_eq!(
        inbound.inbound[0]
            .items
            .iter()
            .map(|item| (item.id, item.hop))
            .collect::<Vec<_>>(),
        [(3, 1), (2, 2), (1, 3)]
    );
}

#[test]
fn direction_selects_one_half_and_leaves_the_other_empty_not_absent() {
    let inbound_only = expand_graph_at(Direction::In, 2, None);
    assert_eq!(inbound_only.outbound, Vec::new());
    assert_eq!(inbound_only.inbound.len(), 2);

    let outbound_only = expand_graph_at(Direction::Out, 2, None);
    assert_eq!(outbound_only.inbound, Vec::new());
    assert_eq!(outbound_only.outbound.len(), 2);
}

#[test]
fn the_edge_kind_filter_is_all_or_nothing() {
    let references = ["references".to_owned()];
    let filtered = expand_graph_at(Direction::Out, 2, Some(&references));
    assert_eq!(filtered.outbound.len(), 1);
    assert_eq!(filtered.outbound[0].kind, "references");
    assert_eq!(
        expand_graph_at(Direction::Out, 2, Some(&[])).outbound.len(),
        2
    );
}

#[test]
fn output_is_unbounded_by_design_seven_neighbours_stay_seven() {
    let hub = node_of(20, "hub", "src/hub.ts::hub", "src/hub.ts");
    let spokes: Vec<NodeRow> = (0..7)
        .map(|index| {
            node_of(
                21 + index,
                &format!("spoke{index}"),
                &format!("src/hub.ts::spoke{index}"),
                "src/hub.ts",
            )
        })
        .collect();
    let edges: Vec<NeighbourEdge> = spokes
        .iter()
        .enumerate()
        .map(|(index, spoke)| {
            link(
                edge_of(
                    100 + i64::try_from(index).expect("a small spoke list"),
                    hub.id,
                    spoke.id,
                    "calls",
                    "extracted",
                    i64::try_from(index).expect("a small spoke list") + 1,
                ),
                spoke.clone(),
            )
        })
        .collect();
    let hub_graph = FakeGraph {
        outbound: HashMap::from([(hub.id, edges)]),
        inbound: HashMap::new(),
    };
    let grouped = expand(&hub_graph, hub.id, Direction::Out, 1, None).expect("expand");
    assert_eq!(grouped.outbound[0].items.len(), 7);
}

#[test]
fn a_node_reachable_twice_is_reported_once_at_the_first_hop() {
    let twice = FakeGraph {
        outbound: HashMap::from([(
            1,
            vec![
                link(edge_of(1, 1, 2, "calls", "extracted", 10), caller()),
                link(edge_of(2, 1, 2, "references", "inferred", 11), caller()),
            ],
        )]),
        inbound: HashMap::new(),
    };
    let result = expand(&twice, 1, Direction::Out, 2, None).expect("expand");
    let ids: Vec<i64> = result
        .outbound
        .iter()
        .flat_map(|group| group.items.iter().map(|item| item.id))
        .collect();
    assert_eq!(ids, [2]);
    // Deterministic winner: `calls` sorts before `references`.
    assert_eq!(result.outbound[0].kind, "calls");
    assert_eq!(result.outbound[0].tier, "extracted");
}
