//! The bind pass: the only place a raw edge becomes a real one.
//!
//! Ported from `src/indexer/resolve.ts`. `extract` leaves every edge but containment unbound — an
//! `extracted_candidate` carrying a textual `target`. This module turns those candidates into edges
//! whose two ends are qns that exist in the [`SymbolIndex`], or records why it could not. A site that
//! binds nothing is a fact about the graph ("we could not prove it") and belongs in `unresolved`; a
//! site that binds two things is a fact we refuse to guess at, because a spurious edge is worse than
//! a missing one.
//!
//! Two rules shape everything here:
//! - the binding order is fixed (same file, then import evidence, then a globally unique name, then
//!   an inherited method), and the tier is the evidence that won: `extracted` when the declaration
//!   was found structurally, `inferred` when it was found by name;
//! - no edge is ever emitted whose `src` or `dst` is not a node in the index. The persist pass
//!   therefore inserts without a second existence check, and the foreign keys cannot be violated by
//!   binding.
//!
//! Nothing here reads a file, opens a connection or knows a language: a [`FileUnit`] is the output
//! of one `extract_file` call plus the path it came from, so the pass is a pure function of its
//! input. Edge order is part of the contract — unit order, then raw-edge order inside a unit — and
//! each unit's own extractor rows come before its binding failures, because the cursor that reads
//! the `unresolved` table indexes into this ordering.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Mutex, PoisonError};

use sangkar_core::paths;

use crate::extract::{ExtractResult, ExtractedNode, RawEdge, UnresolvedSite};

/// The two tiers a bound edge can have. `extracted_candidate` never survives this pass.
pub const EDGE_TIERS: [&str; 2] = ["extracted", "inferred"];

const EXTRACTED: &str = EDGE_TIERS[0];
const INFERRED: &str = EDGE_TIERS[1];

/// Why a call, an import or an inheritance base could not be bound. `dynamic` is the extractor's own
/// kind (the source offered no name), kept here so an extractor site passes through unchanged.
///
/// `missing_module` is the read side's only way to tell a broken relative import (a file that is not
/// in the tree) from a module this resolver was never meant to resolve (a package, `node:fs`, or a
/// `#` tsconfig alias): the specifier already said which one it was, and recording both as `dynamic`
/// made a real defect indistinguishable from an out-of-tree module.
pub const BIND_EVIDENCE_KINDS: [&str; 4] = [
    "receiver_type_unknown",
    "ambiguous",
    "dynamic",
    "missing_module",
];

const RECEIVER_TYPE_UNKNOWN: &str = BIND_EVIDENCE_KINDS[0];
const AMBIGUOUS: &str = BIND_EVIDENCE_KINDS[1];
const DYNAMIC: &str = BIND_EVIDENCE_KINDS[2];
const MISSING_MODULE: &str = BIND_EVIDENCE_KINDS[3];

/// Reportable symbols, the only nodes a name can bind to. `file`, `module` and `endpoint` are
/// deliberately excluded: a call to `base` must not resolve to the module node `base`, and an
/// endpoint is a route, not a callee.
const DECLARATION_KINDS: [&str; 4] = ["class", "function", "method", "test"];

/// Kinds an inheritance base may name: a class, or a namespace standing in for an interface.
const TYPE_KINDS: [&str; 2] = ["class", "module"];

/// Receivers that name the enclosing class rather than a value with a type of its own.
const SELF_RECEIVERS: [&str; 2] = ["self", "this"];

/// Stems of the files a directory import lands in, per the module systems of the twelve grammars.
const INDEX_MODULE_STEMS: [&str; 3] = ["index", "__init__", "mod"];

/// Everything one file contributes to the graph before anything is bound. `rel_path` is repo-relative
/// and already normalised, and the extractor's own `unresolved` sites ride along so the pass is the
/// single producer of the final unresolved list.
#[derive(Clone, Debug, PartialEq)]
pub struct FileUnit {
    pub rel_path: String,
    pub nodes: Vec<ExtractedNode>,
    pub raw_edges: Vec<RawEdge>,
    pub unresolved: Vec<UnresolvedSite>,
}

/// Pairs one extractor result with its path, so a caller never assembles a unit by hand.
#[must_use]
pub fn file_unit(rel_path: &str, result: &ExtractResult) -> FileUnit {
    FileUnit {
        rel_path: paths::normalize_rel(rel_path),
        nodes: result.nodes.clone(),
        raw_edges: result.raw_edges.clone(),
        unresolved: result.unresolved.clone(),
    }
}

/// One resolved edge, keyed by qn on both ends. `file` is the unit the site came from, so persist can
/// find the `file_id` without re-deriving it from `src` — a call edge's source can be a symbol the
/// site does not sit in once binding jumps to a superclass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedEdge {
    pub file: String,
    pub kind: String,
    pub src: String,
    pub dst: String,
    pub tier: String,
    pub line: u32,
    pub local_name: String,
    pub evidence: String,
}

/// One site this pass could not bind, with the reason. `raw_target` is the name as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedBinding {
    pub file: String,
    pub line: u32,
    pub raw_target: String,
    pub evidence_kind: String,
    pub evidence: String,
}

/// Everything one pass produced. Both lists are ordered, and the ordering is the contract a cursor
/// into the stored tables depends on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BindResult {
    pub edges: Vec<ResolvedEdge>,
    pub unresolved: Vec<UnresolvedBinding>,
}

/// A base named by an inheritance clause, and the file whose import table could type it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseReference {
    pub name: String,
    pub file: String,
}

/// One name an import statement makes visible in a file: `local_name = import(module)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportBinding {
    pub module: String,
    pub local_name: String,
}

/// The whole graph's symbol table, built once from the units. Construction owns every derived table
/// the binding order needs — per-file declarations, per-file import tables, inheritance bases and the
/// extensionless path of each unit — so no caller has to assemble index internals.
pub struct SymbolIndex {
    /// qn → the first node that claimed it, which is the symbol `UNIQUE (qn)` keeps.
    nodes: HashMap<String, ExtractedNode>,
    /// Rel path → qns of that file's nodes, in declaration order.
    by_file: HashMap<String, Vec<String>>,
    /// Name → qns of the reportable declarations carrying it, across the whole index.
    by_name: HashMap<String, Vec<String>>,
    /// Rel path → local name → the import bindings that make it visible.
    imports: HashMap<String, HashMap<String, Vec<ImportBinding>>>,
    /// Class qn → the bases its inheritance clauses named, in source order.
    bases: HashMap<String, Vec<BaseReference>>,
    /// Extensionless rel path → the units it could name, for module-string resolution.
    stems: HashMap<String, Vec<String>>,
    /// `specifier\0importer` → the units it could name, so two importers never share an entry.
    ///
    /// The memo is the TypeScript class's, kept because `module_candidates` is called once per
    /// import edge and once per imported name, and each miss scans every stem. A `Mutex` rather than
    /// a `RefCell` because the answers do not depend on the caller: two threads sharing one index
    /// must not each pay for the same scan, and the borrow checker cannot prove a `&self` cache
    /// otherwise.
    candidates: Mutex<HashMap<String, Vec<String>>>,
}

impl From<&[FileUnit]> for SymbolIndex {
    /// Builds the index from the units about to be bound. The only public way in.
    fn from(units: &[FileUnit]) -> Self {
        let mut index = Self {
            nodes: HashMap::new(),
            by_file: HashMap::new(),
            by_name: HashMap::new(),
            imports: HashMap::new(),
            bases: HashMap::new(),
            stems: HashMap::new(),
            candidates: Mutex::new(HashMap::new()),
        };

        for unit in units {
            let rel_path = paths::normalize_rel(&unit.rel_path);
            let mut qns = Vec::with_capacity(unit.nodes.len());
            for node in &unit.nodes {
                qns.push(node.qn.clone());
                // The first declaration of a qn wins: the extractor already refuses to emit a
                // second one, so this only keeps a hand-built unit from replacing a symbol.
                if !index.nodes.contains_key(&node.qn) {
                    index.nodes.insert(node.qn.clone(), node.clone());
                }
                if is_declaration(&node.kind) {
                    push(&mut index.by_name, node.name.clone(), node.qn.clone());
                }
            }
            index.by_file.insert(rel_path.clone(), qns);

            let stem = stem_of(&rel_path);
            push(&mut index.stems, stem.clone(), rel_path.clone());
            if let Some(slash) = stem.rfind('/') {
                let base = &stem[slash + 1..];
                if INDEX_MODULE_STEMS.contains(&base) {
                    push(&mut index.stems, stem[..slash].to_owned(), rel_path.clone());
                }
            }
        }

        for unit in units {
            let rel_path = paths::normalize_rel(&unit.rel_path);
            let mut table: HashMap<String, Vec<ImportBinding>> = HashMap::new();
            for edge in &unit.raw_edges {
                match edge.kind.as_str() {
                    "imports" => {
                        // `import os` records no local name; it binds `os`, the module's own last
                        // segment.
                        let local_name = if edge.local_name.is_empty() {
                            last_segment(&edge.target)
                        } else {
                            edge.local_name.clone()
                        };
                        if !local_name.is_empty() {
                            let binding = ImportBinding {
                                module: edge.target.clone(),
                                local_name: local_name.clone(),
                            };
                            push(&mut table, local_name, binding);
                        }
                    }
                    "inherits" | "implements" => {
                        let base = BaseReference {
                            name: edge.target.clone(),
                            file: rel_path.clone(),
                        };
                        push(&mut index.bases, edge.src.clone(), base);
                    }
                    _ => {}
                }
            }
            index.imports.insert(rel_path, table);
        }

        index
    }
}

impl SymbolIndex {
    /// The node a qn names, if any unit declared it.
    #[must_use]
    pub fn node(&self, qn: &str) -> Option<&ExtractedNode> {
        self.nodes.get(qn)
    }

    /// The declarations one file holds, in source order.
    #[must_use]
    pub fn nodes_in(&self, rel_path: &str) -> Vec<&ExtractedNode> {
        let key = paths::normalize_rel(rel_path);
        self.by_file
            .get(&key)
            .map_or_else(Vec::new, |qns| self.materialize(qns))
    }

    /// Every reportable declaration in the graph carrying `name`.
    #[must_use]
    pub fn nodes_named(&self, name: &str) -> Vec<&ExtractedNode> {
        self.by_name
            .get(name)
            .map_or_else(Vec::new, |qns| self.materialize(qns))
    }

    /// The unit's own file node; every `imports` edge targets one of these.
    #[must_use]
    pub fn file_node(&self, rel_path: &str) -> Option<&ExtractedNode> {
        self.nodes_in(rel_path)
            .into_iter()
            .find(|node| node.kind == "file")
    }

    /// The import bindings one file's table holds under `local_name`; the branch that needs it asks
    /// by name rather than walking a whole table it cannot name.
    #[must_use]
    pub fn import_bindings(&self, rel_path: &str, local_name: &str) -> &[ImportBinding] {
        let key = paths::normalize_rel(rel_path);
        self.imports
            .get(&key)
            .and_then(|table| table.get(local_name))
            .map_or(&[], Vec::as_slice)
    }

    /// The bases a class's inheritance clauses named, whether or not they resolved.
    #[must_use]
    pub fn bases_of(&self, qn: &str) -> &[BaseReference] {
        self.bases.get(qn).map_or(&[], Vec::as_slice)
    }

    /// Every unit a module string could name, sorted. An empty list is "nothing matched" and two or
    /// more paths are "the stem is not enough to decide", which are different facts: the first is a
    /// module the resolver cannot see (a package, a stdlib import), the second is a tree defect the
    /// caller can report along with the competitors. Python's `a.b` and TypeScript's `./a/b` both
    /// become `a/b`; `index` and `__init__` answering for their directory is the whole of
    /// directory-import support.
    ///
    /// `from_file` is the importing unit, and for a relative specifier it is what decides the
    /// meaning: `./x.js` from `src/a/b.ts` is the stem `src/a/x`, so that exact stem is searched
    /// before the global suffix search below — without it every `./y` would tie between all the units
    /// whose stem happens to end in `y`. An absolute or escaping specifier resolves to nothing and
    /// falls through to the module-string reading, which is what keeps `../../../x` from binding on a
    /// coincidence.
    #[must_use]
    pub fn module_candidates(&self, module: &str, from_file: Option<&str>) -> Vec<String> {
        let key = format!("{module}\u{0}{}", from_file.unwrap_or(""));
        if let Some(cached) = self
            .candidates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return cached.clone();
        }

        let relative = from_file.is_some() && is_relative_specifier(module);
        let local = if relative {
            from_file
                .and_then(|importer| paths::resolve_from(importer, module))
                .map_or_else(Vec::new, |stem| self.matching_stems(&stem))
        } else {
            Vec::new()
        };
        let found = if local.is_empty() {
            self.matching_stems(&module_path(module))
        } else {
            local
        };

        self.candidates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, found.clone());
        found
    }

    /// The unit a module string names, or `None` when the string matches no unit or more than one — a
    /// module string that matches two files cannot be resolved and must not be guessed. Expressed
    /// over [`Self::module_candidates`] so the two can never disagree about which units are in play.
    #[must_use]
    pub fn module_file(&self, module: &str, from_file: Option<&str>) -> Option<String> {
        let candidates = self.module_candidates(module, from_file);
        if candidates.len() == 1 {
            return candidates.into_iter().next();
        }
        None
    }

    /// The units an extensionless rel path could name, sorted, or an empty list for no stem at all.
    /// An exact single entry wins outright; otherwise the suffix match collects every unit the stem
    /// could be the tail of.
    fn matching_stems(&self, key: &str) -> Vec<String> {
        if key.is_empty() {
            return Vec::new();
        }
        let mut found: Vec<String> = Vec::new();
        match self.stems.get(key) {
            Some(entry) if entry.len() == 1 => found.extend(entry.iter().cloned()),
            _ => {
                let suffix = format!("/{key}");
                // A `BTreeSet` because the TypeScript collected into a `Set` and sorted afterwards:
                // duplicates across entries collapse, and the order is the sorted one either way.
                let mut matches: BTreeSet<&String> = BTreeSet::new();
                for (stem, entry) in &self.stems {
                    if stem == key || stem.ends_with(&suffix) {
                        matches.extend(entry);
                    }
                }
                found.extend(matches.into_iter().cloned());
            }
        }
        // Sorted because the candidates end up in evidence text, and two runs over the same tree must
        // produce byte-identical rows.
        found.sort();
        found
    }

    fn materialize(&self, qns: &[String]) -> Vec<&ExtractedNode> {
        qns.iter().filter_map(|qn| self.nodes.get(qn)).collect()
    }
}

/// Binds every unit against one index. Edges come out in unit order and, inside a unit, in raw-edge
/// order, so two runs over the same input are byte-identical; duplicate edges (the same site seen
/// twice) are collapsed on the key the `edge` table is unique on.
#[must_use]
pub fn bind(units: &[FileUnit], index: &SymbolIndex) -> BindResult {
    let mut sink = Sink::new(index);
    for unit in units {
        let file = paths::normalize_rel(&unit.rel_path);
        // The extractor's own nameless sites come first: the pass is the single producer of the
        // stored list, so it appends to them rather than replacing them.
        for site in &unit.unresolved {
            sink.record_extractor_site(&file, site);
        }
        for raw in &unit.raw_edges {
            bind_raw(&file, raw, &mut sink);
        }
    }
    sink.finish()
}

/// The edges and failures one pass accumulated, with the two operations every branch uses: emit an
/// edge, or record why there is none.
struct Sink<'a> {
    index: &'a SymbolIndex,
    edges: Vec<ResolvedEdge>,
    unresolved: Vec<UnresolvedBinding>,
    seen: HashSet<String>,
}

impl<'a> Sink<'a> {
    fn new(index: &'a SymbolIndex) -> Self {
        Self {
            index,
            edges: Vec::new(),
            unresolved: Vec::new(),
            seen: HashSet::new(),
        }
    }

    /// Adds an edge if both ends exist and the site is new.
    fn emit(&mut self, file: &str, raw: &RawEdge, dst: &str, tier: &str) {
        // The invariant this pass exists to keep: an edge never dangles.
        if self.index.node(&raw.src).is_none() || self.index.node(dst).is_none() {
            return;
        }
        let key = format!(
            "{file}\u{0}{}\u{0}{}\u{0}{dst}\u{0}{}\u{0}{}",
            raw.kind, raw.src, raw.line, raw.local_name
        );
        if !self.seen.insert(key) {
            return;
        }
        self.edges.push(ResolvedEdge {
            file: file.to_owned(),
            kind: raw.kind.clone(),
            src: raw.src.clone(),
            dst: dst.to_owned(),
            tier: tier.to_owned(),
            line: raw.line,
            local_name: raw.local_name.clone(),
            evidence: evidence_of(raw),
        });
    }

    fn fail(&mut self, file: &str, raw: &RawEdge, evidence_kind: &str, evidence: String) {
        self.unresolved.push(UnresolvedBinding {
            file: file.to_owned(),
            line: raw.line,
            raw_target: raw.target.clone(),
            evidence_kind: evidence_kind.to_owned(),
            evidence,
        });
    }

    /// A site the extractor could not even name, passed through unchanged.
    fn record_extractor_site(&mut self, file: &str, site: &UnresolvedSite) {
        self.unresolved.push(UnresolvedBinding {
            file: file.to_owned(),
            line: site.line,
            raw_target: site.raw_target.clone(),
            evidence_kind: site.evidence_kind.clone(),
            evidence: site.evidence.clone(),
        });
    }

    fn finish(self) -> BindResult {
        BindResult {
            edges: self.edges,
            unresolved: self.unresolved,
        }
    }
}

/// One raw edge, by kind: containment and inheritance are structural, calls and imports are names.
fn bind_raw(file: &str, raw: &RawEdge, sink: &mut Sink<'_>) {
    let index = sink.index;
    match raw.kind.as_str() {
        "contains" => {
            // The extractor already wrote the child's qn, so containment is evidence, not inference.
            sink.emit(file, raw, &raw.target, EXTRACTED);
        }
        "inherits" | "implements" => match resolve_type_name(file, &raw.target, index) {
            NameOutcome::Hit(qn) => sink.emit(file, raw, &qn, EXTRACTED),
            // A type that exists nowhere in the indexed tree is outside the graph, which is
            // `dynamic`; a name two types carry is `ambiguous`.
            NameOutcome::Ambiguous => sink.fail(file, raw, AMBIGUOUS, evidence_of(raw)),
            NameOutcome::Missing => sink.fail(file, raw, DYNAMIC, evidence_of(raw)),
        },
        "references" => {
            // Exactly one node, or nothing: a reference is a value position with no receiver
            // evidence, so two candidates are indistinguishable and the edge is dropped *silently* —
            // recording it would claim a name was unresolvable when the name is merely non-unique.
            let hits = index.nodes_named(&raw.target);
            if let [only] = hits.as_slice() {
                let dst = only.qn.clone();
                sink.emit(file, raw, &dst, INFERRED);
            }
        }
        "calls" => match bind_call(file, raw, index) {
            CallOutcome::Edge { dst, tier } => sink.emit(file, raw, &dst, tier),
            CallOutcome::Unresolved(evidence_kind) => {
                sink.fail(file, raw, evidence_kind, evidence_of(raw));
            }
        },
        "imports" => {
            // A module string names a unit, not a declaration: the same-file, unique-name and
            // inheritance steps have nothing to look at, so an import binds to the target file node
            // at `extracted`, and the reason a failed one failed is read off the candidate set.
            let candidates = index.module_candidates(&raw.target, Some(file));
            if candidates.len() == 1
                && let Some(target) = index.file_node(&candidates[0])
            {
                let dst = target.qn.clone();
                sink.emit(file, raw, &dst, EXTRACTED);
                return;
            }
            import_failure(sink, file, raw, &candidates);
        }
        _ => {}
    }
}

/// What one call site resolved to: a target with the evidence's tier, or the cause there is none.
enum CallOutcome {
    Edge { dst: String, tier: &'static str },
    Unresolved(&'static str),
}

/// The four-step binding order, returning either a target qn or the reason there is none.
fn bind_call(file: &str, raw: &RawEdge, index: &SymbolIndex) -> CallOutcome {
    // 1. Exact qn in the same file: the strongest evidence there is, so it is checked first and an
    //    ambiguity here ends the search rather than weakening to a wider guess.
    let same = same_file_hits(file, raw, index);
    if let [only] = same.as_slice() {
        return CallOutcome::Edge {
            dst: only.clone(),
            tier: EXTRACTED,
        };
    }
    if same.len() > 1 {
        return CallOutcome::Unresolved(AMBIGUOUS);
    }

    // 2. A name this file imported: the import statement is the evidence, so it is still
    //    `extracted`.
    let imported = import_hits(file, &raw.target, index);
    if let [only] = imported.as_slice() {
        return CallOutcome::Edge {
            dst: only.clone(),
            tier: EXTRACTED,
        };
    }
    if imported.len() > 1 {
        return CallOutcome::Unresolved(AMBIGUOUS);
    }

    // 3. A name that occurs exactly once in the whole graph: unique, therefore `inferred`.
    let global = index.nodes_named(&raw.target);
    if let [only] = global.as_slice() {
        return CallOutcome::Edge {
            dst: only.qn.clone(),
            tier: INFERRED,
        };
    }

    // 4. A superclass method for a receiver whose class we could establish: still `inferred`,
    //    because the receiver's type came from the class's own declaration, not from the call site.
    let inherited = inherited_hits(file, raw, index);
    if let [only] = inherited.as_slice() {
        return CallOutcome::Edge {
            dst: only.clone(),
            tier: INFERRED,
        };
    }
    if inherited.len() > 1 {
        return CallOutcome::Unresolved(AMBIGUOUS);
    }

    CallOutcome::Unresolved(fallback_evidence(raw.receiver.as_deref(), global.len()))
}

/// Why a call stayed unbound. An untyped receiver is the root cause whenever one was written: the
/// candidates may well differ by type, which is exactly what we could not establish. Without a
/// receiver the reason is the candidate set itself — several matches, or none at all.
fn fallback_evidence(receiver: Option<&str>, global_hits: usize) -> &'static str {
    if receiver.is_some() {
        return RECEIVER_TYPE_UNKNOWN;
    }
    if global_hits > 1 {
        return AMBIGUOUS;
    }
    DYNAMIC
}

/// Step 1: the target's exact qn, using the receiver as a scope when it names one.
fn same_file_hits(file: &str, raw: &RawEdge, index: &SymbolIndex) -> Vec<String> {
    let Some(receiver) = raw.receiver.as_deref() else {
        return index
            .nodes_in(file)
            .into_iter()
            .filter(|node| node.name == raw.target && is_declaration(&node.kind))
            .map(|node| node.qn.clone())
            .collect();
    };
    let Some(scope) = receiver_scope(&raw.src, receiver, index) else {
        return Vec::new();
    };
    let exact = paths::qn(file, &scope, &raw.target);
    if index.node(&exact).is_some() {
        return vec![exact];
    }
    Vec::new()
}

/// The qn scope a receiver names inside `src`. `self`/`this` mean the enclosing declaration's scope,
/// which for a method qn (`path::Class.method`) is the class; any other receiver is taken as the
/// scope as written, so `Widget.build()` finds `path::Widget.build` and a variable simply misses.
fn receiver_scope(src: &str, receiver: &str, index: &SymbolIndex) -> Option<String> {
    if !SELF_RECEIVERS.contains(&receiver) {
        return Some(receiver.to_owned());
    }
    let node = index.node(src)?;
    if node.kind != "method" {
        return None;
    }
    let scope = scope_of(&node.qn);
    if scope.is_empty() {
        return None;
    }
    Some(scope)
}

/// Step 2: a name visible through this file's import table, resolved into the importing unit.
fn import_hits(file: &str, name: &str, index: &SymbolIndex) -> Vec<String> {
    let mut out = Vec::new();
    for binding in index.import_bindings(file, name) {
        let Some(module_rel) = index.module_file(&binding.module, Some(file)) else {
            continue;
        };
        for node in index.nodes_in(&module_rel) {
            if node.name == binding.local_name && is_declaration(&node.kind) {
                out.push(node.qn.clone());
            }
        }
    }
    out
}

/// Step 4: the methods a receiver's class inherits under the target's name. The chain is walked by
/// name because that is all an inheritance clause records, and it is cycle-safe: a class already seen
/// is never expanded again.
fn inherited_hits(file: &str, raw: &RawEdge, index: &SymbolIndex) -> Vec<String> {
    let Some(start) = receiver_class(file, raw, index) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    seen.insert(start.qn.clone());
    let mut frontier: Vec<BaseReference> = index.bases_of(&start.qn).to_vec();
    while !frontier.is_empty() {
        let mut next = Vec::new();
        for base in &frontier {
            let Some(ancestor) = resolve_class(&base.name, &base.file, index) else {
                continue;
            };
            if !seen.insert(ancestor.qn.clone()) {
                continue;
            }
            for method in methods_of(ancestor, index) {
                if method.name == raw.target {
                    out.push(method.qn.clone());
                }
            }
            next.extend_from_slice(index.bases_of(&ancestor.qn));
        }
        frontier = next;
    }
    out
}

/// The class a receiver names, or `None` when its type cannot be established from the graph.
fn receiver_class<'a>(
    file: &str,
    raw: &'a RawEdge,
    index: &'a SymbolIndex,
) -> Option<&'a ExtractedNode> {
    let receiver = raw.receiver.as_deref()?;
    if !SELF_RECEIVERS.contains(&receiver) {
        return resolve_class(receiver, file, index);
    }
    let node = index.node(&raw.src)?;
    if node.kind != "method" {
        return None;
    }
    let scope = scope_of(&node.qn);
    if scope.is_empty() {
        return None;
    }
    index.node(&paths::qn(file, "", &scope))
}

/// A class or namespace by name: the same file first, then a name unique in the whole index.
fn resolve_class<'a>(name: &str, file: &str, index: &'a SymbolIndex) -> Option<&'a ExtractedNode> {
    let local: Vec<&ExtractedNode> = index
        .nodes_in(file)
        .into_iter()
        .filter(|node| node.name == name && is_type(&node.kind))
        .collect();
    if let [only] = local.as_slice() {
        return Some(only);
    }
    if !local.is_empty() {
        return None;
    }
    let global: Vec<&ExtractedNode> = index
        .nodes_named(name)
        .into_iter()
        .filter(|node| is_type(&node.kind))
        .collect();
    if let [only] = global.as_slice() {
        return Some(only);
    }
    None
}

/// The direct methods of one class: qns one segment below the class's own qn.
fn methods_of<'a>(class: &ExtractedNode, index: &'a SymbolIndex) -> Vec<&'a ExtractedNode> {
    let prefix = format!("{}.", class.qn);
    index
        .nodes_in(file_of(&class.qn))
        .into_iter()
        .filter(|node| {
            node.kind == "method"
                && node.qn.starts_with(&prefix)
                && !node.qn[prefix.len()..].contains('.')
        })
        .collect()
}

/// What an inheritance base name resolved to.
enum NameOutcome {
    Hit(String),
    Ambiguous,
    Missing,
}

/// An inheritance base or an `implements` entry: a type name, resolved like any other declaration.
fn resolve_type_name(file: &str, name: &str, index: &SymbolIndex) -> NameOutcome {
    let local: Vec<&ExtractedNode> = index
        .nodes_in(file)
        .into_iter()
        .filter(|node| node.name == name && is_type(&node.kind))
        .collect();
    if let [only] = local.as_slice() {
        return NameOutcome::Hit(only.qn.clone());
    }
    if !local.is_empty() {
        return NameOutcome::Ambiguous;
    }
    let global: Vec<&ExtractedNode> = index
        .nodes_named(name)
        .into_iter()
        .filter(|node| is_type(&node.kind))
        .collect();
    if let [only] = global.as_slice() {
        return NameOutcome::Hit(only.qn.clone());
    }
    if !global.is_empty() {
        return NameOutcome::Ambiguous;
    }
    NameOutcome::Missing
}

/// Why an import did not bind, from the candidate set and the specifier's shape.
///
/// A tie is `ambiguous` and names the competitors in the evidence, because the fix lives in the tree
/// (two files sharing a stem) and a count alone would not say which. A relative specifier that
/// matched nothing is `missing_module`: the file it points at is not in the tree. Anything else is
/// `dynamic`, unchanged — a bare package, `node:`/`bun:`, or a `#` alias, which is a tsconfig path
/// mapping this resolver cannot see and therefore must not report as a defect in the tree.
fn import_failure(sink: &mut Sink<'_>, file: &str, raw: &RawEdge, candidates: &[String]) {
    if candidates.len() > 1 {
        let evidence = format!(
            "{} candidates: {}",
            evidence_of(raw),
            candidates.join(" | ")
        );
        sink.fail(file, raw, AMBIGUOUS, evidence);
        return;
    }
    if candidates.is_empty() && is_relative_specifier(&raw.target) {
        let evidence = format!("{} (no file matched)", evidence_of(raw));
        sink.fail(file, raw, MISSING_MODULE, evidence);
        return;
    }
    // One candidate with no file node cannot happen (every unit carries one); it stays `dynamic`
    // rather than claiming a defect, so the fallthrough is honest instead of assumed.
    sink.fail(file, raw, DYNAMIC, evidence_of(raw));
}

/// The two forms that mean "a path in this tree", so a miss is a defect rather than an import.
fn is_relative_specifier(module: &str) -> bool {
    module.starts_with("./") || module.starts_with("../")
}

fn is_declaration(kind: &str) -> bool {
    DECLARATION_KINDS.contains(&kind)
}

fn is_type(kind: &str) -> bool {
    TYPE_KINDS.contains(&kind)
}

/// The raw edge's evidence text; a hand-built edge may carry none, which stores as an empty string.
fn evidence_of(raw: &RawEdge) -> String {
    raw.evidence.clone().unwrap_or_default()
}

fn push<K, V>(map: &mut HashMap<K, Vec<V>>, key: K, value: V)
where
    K: std::hash::Hash + Eq,
{
    map.entry(key).or_default().push(value);
}

/// `path::A.b` → `A`; `path::x` → `""`. The scope is what a same-file exact lookup composes.
fn scope_of(qn: &str) -> String {
    let Some((_, local)) = qn.split_once("::") else {
        return String::new();
    };
    match local.rfind('.') {
        Some(dot) => local[..dot].to_owned(),
        None => String::new(),
    }
}

/// `path::A.b` → `path`.
fn file_of(qn: &str) -> &str {
    match qn.split_once("::") {
        Some((path, _)) => path,
        None => qn,
    }
}

fn last_segment(text: &str) -> String {
    text.split(['.', '/', '\\', ':'])
        .rfind(|part| !part.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// `src/a/b.py` → `src/a/b`; a name with no extension is left alone.
fn stem_of(rel_path: &str) -> String {
    let slash = rel_path.rfind('/');
    let dot = rel_path.rfind('.');
    match (slash, dot) {
        (_, None) => rel_path.to_owned(),
        (Some(slash), Some(dot)) if dot > slash => rel_path[..dot].to_owned(),
        (None, Some(dot)) => rel_path[..dot].to_owned(),
        _ => rel_path.to_owned(),
    }
}

/// A module string as repository-relative path segments: `a.b` → `a/b`, `./a/b` → `a/b`,
/// `src/helpers.rb` → `src/helpers`. Only the first form needs the dot rewrite, and only the second
/// needs the relative prefix stripped, so the two cases are decided by the separator they already
/// carry.
fn module_path(module: &str) -> String {
    let slashed = module.replace('\\', "/");
    if slashed.contains('/') {
        strip_extension(&paths::normalize_rel(&slashed))
    } else {
        module.replace('.', "/")
    }
}

/// Drops a trailing `.[A-Za-z0-9]+`, the extension a module path is written without.
fn strip_extension(path: &str) -> String {
    let Some(dot) = path.rfind('.') else {
        return path.to_owned();
    };
    let tail = &path[dot + 1..];
    if !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return path[..dot].to_owned();
    }
    path.to_owned()
}

#[cfg(test)]
mod tests {
    use super::{file_of, last_segment, module_path, scope_of, stem_of};

    /// The qn accessors the binding branches share: one separator, one dot, no invented scope.
    #[test]
    fn a_qn_splits_into_its_file_and_its_scope() {
        assert_eq!(file_of("src/a.py::A.b"), "src/a.py");
        assert_eq!(file_of("src/a.py::A"), "src/a.py");
        assert_eq!(scope_of("src/a.py::A.b"), "A");
        assert_eq!(scope_of("src/a.py::A.B.c"), "A.B");
        assert_eq!(scope_of("src/a.py::x"), "");
        assert_eq!(scope_of("no-separator"), "");
    }

    /// A module string is a path in exactly the two shapes the grammars write it in.
    #[test]
    fn a_module_string_becomes_a_path_by_the_separator_it_carries() {
        assert_eq!(module_path("a.b"), "a/b");
        assert_eq!(module_path("./a/b"), "a/b");
        assert_eq!(module_path("src/helpers.rb"), "src/helpers");
        // A dot-only name takes the dot rewrite, which is what the TypeScript did: the branch is
        // decided by whether the string carries a separator, not by what the dots mean.
        assert_eq!(module_path(".hidden"), "/hidden");
        assert_eq!(last_segment("os.path"), "path");
        assert_eq!(last_segment("A::B"), "B");
    }

    /// `dot > slash` is the whole rule: a dot before the last slash belongs to a directory.
    #[test]
    fn only_an_extension_after_the_last_slash_is_stripped() {
        assert_eq!(stem_of("src/a/b.py"), "src/a/b");
        assert_eq!(stem_of("a.py"), "a");
        assert_eq!(stem_of("src/v1.2/a"), "src/v1.2/a");
        assert_eq!(stem_of("a"), "a");
    }
}
