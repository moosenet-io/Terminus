//! Atlas deterministic node/edge extraction (KGRAPH-02).
//!
//! Reuses the same tree-sitter emit-kind whitelist Harmony's `repo_map.rs` uses
//! to decide what is a node (top-level items + impl/trait methods), but emits
//! [`KgNode`]s and EXTRACTED [`KgEdge`]s instead of a text section. Unlike
//! `repo_map`, it DOES descend into function bodies — but only to capture the
//! `Calls` they make; emission stays gated so a local item inside a body is
//! never surfaced as a node (we keep "top-level items + methods only").
//!
//! Rust only in this item (Python/TS grammars follow in a later widening item).
//! Everything here is a pure, in-memory parse: no I/O, no networking, no
//! secrets. The caller (KGRAPH-03 store / KGRAPH-10 build) is responsible for
//! reading files from an allowlisted inspection worktree.
//!
//! ## Two passes
//! 1. Parse every file, emit its nodes (a `Module` node per file, plus each
//!    top-level item and impl/trait method), and record *edge candidates*
//!    `(source_id, target_simple_name, kind)` for imports and calls.
//! 2. Build a global `name -> id` index over all nodes, then resolve each
//!    candidate against it (same-file first, then a unique global match).
//!    Unresolved candidates are dropped — inference of ambiguous targets is
//!    KGRAPH-04's job, and precise cross-file resolution is KGRAPH-11's
//!    (stack-graphs). Structural `Contains` edges are always intra-file and
//!    resolvable immediately.

use std::collections::HashMap;

use tree_sitter::{Node, Parser};

use super::model::{Confidence, EdgeKind, KgEdge, KgNode, KnowledgeGraph, NodeKind};
use crate::error::ToolError;

/// Rust top-level item kinds we emit as nodes.
fn emit_kind(kind: &str) -> Option<NodeKind> {
    match kind {
        "function_item" => Some(NodeKind::Function),
        "struct_item" | "union_item" => Some(NodeKind::Struct),
        "enum_item" => Some(NodeKind::Enum),
        "trait_item" => Some(NodeKind::Trait),
        // type aliases, consts, statics, macros map onto Module-adjacent
        // "module member" nodes; kind Struct/Function would be misleading, so we
        // treat them as their own light categories via Module? Keep it simple:
        // model them as Function-like leaves is wrong. Represent as best fit.
        "type_item" => Some(NodeKind::Struct),
        "macro_definition" => Some(NodeKind::Function),
        _ => None,
    }
}

/// Derive a Rust module FQN from a repo-relative path.
/// `src/scribe/graph/model.rs` -> `crate::scribe::graph::model`;
/// `src/scribe/mod.rs` -> `crate::scribe`; `src/lib.rs`/`src/main.rs` -> `crate`.
pub fn module_fqn(path: &str) -> String {
    let p = path.trim_start_matches("./");
    let stem = p.strip_suffix(".rs").unwrap_or(p);
    let mut parts: Vec<&str> = stem.split('/').collect();
    // Drop a leading crate-root dir like `src` (also common: crate name dirs
    // are kept; `src` is the ubiquitous one worth normalizing).
    if parts.first() == Some(&"src") {
        parts.remove(0);
    }
    // `mod`, `lib`, `main` file stems are the module's directory, not a segment.
    if matches!(parts.last().copied(), Some("mod") | Some("lib") | Some("main")) {
        parts.pop();
    }
    let mut fqn = String::from("crate");
    for seg in parts {
        if !seg.is_empty() {
            fqn.push_str("::");
            fqn.push_str(seg);
        }
    }
    fqn
}

/// The last `::`-segment of a possibly-scoped name (`a::b::C` -> `C`).
fn last_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name).trim()
}

/// The base type name: last `::`-segment with any generic argument list
/// stripped (`std::vec::Vec<T>` -> `Vec`, `Cache<K, V>` -> `Cache`). Without
/// this, generic-type impls would FQN a method as `crate::m::Cache<T>::get`,
/// which never matches the `crate::m::Cache` struct node.
fn base_type_name(s: &str) -> String {
    last_segment(s)
        .split('<')
        .next()
        .unwrap_or("")
        .trim()
        .to_string()
}

struct FileExtract {
    nodes: Vec<KgNode>,
    /// Structural edges, both endpoints intra-file (always resolvable).
    contains: Vec<(String, String)>,
    /// `(source_id, target_simple_name, kind)` to resolve in pass 2.
    candidates: Vec<(String, String, EdgeKind)>,
}

/// Walk context threaded through the recursion.
struct Ctx<'a> {
    src: &'a [u8],
    path: &'a str,
    module: String,
    out: FileExtract,
}

impl<'a> Ctx<'a> {
    fn text(&self, n: Node) -> String {
        n.utf8_text(self.src).unwrap_or("").to_string()
    }

    fn name_of(&self, n: Node) -> Option<String> {
        n.child_by_field_name("name").map(|c| self.text(c))
    }

    fn push_node(&mut self, id: &str, kind: NodeKind, name: &str, node: Node) {
        let (start, end) = (
            node.start_position().row as u32 + 1,
            node.end_position().row as u32 + 1,
        );
        self.out
            .nodes
            .push(KgNode::new(id, kind, name, self.path).with_span(start, end));
    }
}

/// Recurse the tree. `container_id` is the FQN of the nearest enclosing emitted
/// node used for `Contains` (the module, or an impl/trait type). `enclosing_fn`
/// is the id of the function a `call_expression` should be attributed to.
fn walk(ctx: &mut Ctx, node: Node, container_id: &str, enclosing_fn: Option<&str>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();

        // `use_declaration` -> Imports candidate from the module.
        if kind == "use_declaration" {
            for name in use_targets(ctx, child) {
                ctx.out
                    .candidates
                    .push((ctx.module.clone(), name, EdgeKind::Imports));
            }
            continue;
        }

        // A call inside the current function -> Calls candidate.
        if kind == "call_expression" {
            if let Some(src_id) = enclosing_fn {
                if let Some(callee) = call_callee(ctx, child) {
                    ctx.out
                        .candidates
                        .push((src_id.to_string(), callee, EdgeKind::Calls));
                }
            }
            // still recurse (nested calls in args)
            walk(ctx, child, container_id, enclosing_fn);
            continue;
        }

        if let Some(nk) = emit_kind(kind) {
            if enclosing_fn.is_some() {
                // We are inside a function body (descended for calls): a local
                // item is NOT a graph node. Still recurse to catch its calls.
                walk(ctx, child, container_id, enclosing_fn);
                continue;
            }
            let Some(name) = ctx.name_of(child) else {
                // unnamed item (rare) — recurse but don't emit
                walk(ctx, child, container_id, enclosing_fn);
                continue;
            };
            let id = format!("{container_id}::{name}");
            ctx.push_node(&id, nk, &name, child);
            ctx.out.contains.push((container_id.to_string(), id.clone()));
            // Descend into a function to catch its calls, attributed to it.
            if nk == NodeKind::Function {
                walk(ctx, child, &id, Some(&id));
            } else {
                // struct/enum/trait: recurse for nested items (e.g. trait
                // methods live under a trait_item body handled below).
                walk(ctx, child, &id, enclosing_fn);
            }
            continue;
        }

        // impl_item: attribute its methods to the implemented type's FQN so a
        // method is `Contains`-ed by the type node (which the struct_item
        // emitted elsewhere), not by the module.
        if kind == "impl_item" {
            let type_id = impl_type_fqn(ctx, child).unwrap_or_else(|| container_id.to_string());
            walk(ctx, child, &type_id, enclosing_fn);
            continue;
        }

        // Anything else (blocks, statement/expression nodes, module bodies):
        // recurse so calls and `use`s nested anywhere in a body are found.
        // Emission is still gated on `emit_kind`, so descending into bodies does
        // not surface non-items; it only lets us see the calls they contain.
        walk(ctx, child, container_id, enclosing_fn);
    }
}

/// The FQN of the type an `impl` block is for: `impl Foo` / `impl Trait for Foo`
/// -> `<module>::Foo`.
fn impl_type_fqn(ctx: &Ctx, impl_node: Node) -> Option<String> {
    let ty = impl_node.child_by_field_name("type")?;
    let name = base_type_name(&ctx.text(ty));
    if name.is_empty() {
        return None;
    }
    Some(format!("{}::{}", ctx.module, name))
}

/// Simple-name targets of a `use_declaration` (best-effort: the final
/// identifier(s); grouped `use a::{b, c}` yields `b` and `c`).
fn use_targets(ctx: &Ctx, use_node: Node) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = use_node.walk();
    for child in use_node.named_children(&mut cursor) {
        match child.kind() {
            "scoped_identifier" | "identifier" | "scoped_use_list" => {
                let t = ctx.text(child);
                out.push(last_segment(&t).to_string());
            }
            // `use a::b::C as D` — link to the imported symbol C (the `path`
            // child), not the local alias D.
            "use_as_clause" => {
                if let Some(path) = child.child_by_field_name("path") {
                    out.push(last_segment(&ctx.text(path)).to_string());
                }
            }
            "use_list" => {
                let mut c2 = child.walk();
                for item in child.named_children(&mut c2) {
                    // an aliased item inside a group is itself a use_as_clause
                    let t = if item.kind() == "use_as_clause" {
                        item.child_by_field_name("path")
                            .map(|p| ctx.text(p))
                            .unwrap_or_default()
                    } else {
                        ctx.text(item)
                    };
                    let seg = last_segment(&t);
                    if !seg.is_empty() {
                        out.push(seg.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    out.retain(|s| !s.is_empty() && s != "self" && s != "*");
    out
}

/// The callee of a `call_expression`, keeping a one-level qualifier for scoped
/// calls so resolution can disambiguate (`Foo::new` stays `Foo::new`, not just
/// `new`); an unqualified call stays a bare name; a method call `x.foo()` yields
/// the field name `foo`. Generic args are stripped from every segment.
fn call_callee(ctx: &Ctx, call: Node) -> Option<String> {
    let f = call.child_by_field_name("function")?;
    let seg = match f.kind() {
        "identifier" => ctx.text(f),
        "scoped_identifier" => scoped_tail(&ctx.text(f)),
        "field_expression" => f
            .child_by_field_name("field")
            .map(|c| ctx.text(c))
            .unwrap_or_default(),
        _ => scoped_tail(&ctx.text(f)),
    };
    let seg = seg.trim().to_string();
    if seg.is_empty() {
        None
    } else {
        Some(seg)
    }
}

/// The last one or two `::`-segments of a scoped path, generics stripped:
/// `a::b::Foo::new` -> `Foo::new`, `crate::m::helper` -> `m::helper`,
/// `bare` -> `bare`. The one-level qualifier is what lets a same-named method on
/// a different type resolve unambiguously.
fn scoped_tail(s: &str) -> String {
    let segs: Vec<String> = s
        .split("::")
        .map(|p| p.split('<').next().unwrap_or("").trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    match segs.len() {
        0 => String::new(),
        1 => segs[0].clone(),
        n => format!("{}::{}", segs[n - 2], segs[n - 1]),
    }
}

/// Extract nodes + edge candidates from one Rust file. Returns an empty result
/// (never an error) if the file fails to parse — a malformed file must not sink
/// the batch (mirrors `repo_map.rs`).
fn extract_file(path: &str, source: &str) -> FileExtract {
    let module = module_fqn(path);
    let out = FileExtract {
        nodes: vec![KgNode::new(&module, NodeKind::Module, last_segment(&module), path)],
        contains: Vec::new(),
        candidates: Vec::new(),
    };

    let mut parser = Parser::new();
    if parser.set_language(&tree_sitter_rust::LANGUAGE.into()).is_err() {
        return out;
    }
    let Some(tree) = parser.parse(source, None) else {
        return out;
    };

    let mut ctx = Ctx {
        src: source.as_bytes(),
        path,
        module: module.clone(),
        out,
    };
    walk(&mut ctx, tree.root_node(), &module, None);
    ctx.out
}

/// Build a knowledge graph from a set of `(repo_relative_path, source)` Rust
/// files. Two-pass: emit all nodes, then resolve import/call candidates against
/// the global node set (same-file first, then a unique global match; ambiguous
/// or unknown targets are dropped). All emitted edges are EXTRACTED.
pub fn build_rust_graph(
    project_id: &str,
    files: &[(String, String)],
) -> Result<KnowledgeGraph, ToolError> {
    let mut graph = KnowledgeGraph::new(project_id);

    // Pass 1: nodes.
    let per_file: Vec<FileExtract> = files
        .iter()
        .filter(|(p, _)| p.ends_with(".rs"))
        .map(|(p, s)| extract_file(p, s))
        .collect();

    for fe in &per_file {
        for n in &fe.nodes {
            graph.insert_node(n.clone());
        }
    }

    // name -> ids index (for resolution), and id -> path (for same-file bias).
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    let mut id_path: HashMap<String, String> = HashMap::new();
    for n in graph.nodes() {
        by_name.entry(n.name.clone()).or_default().push(n.id.clone());
        id_path.insert(n.id.clone(), n.path.clone());
    }

    // Pass 2a: structural Contains edges (always resolvable).
    for fe in &per_file {
        for (from, to) in &fe.contains {
            // both endpoints were emitted in pass 1
            let _ = graph.insert_edge(KgEdge::new(from, to, EdgeKind::Contains, Confidence::Extracted));
        }
    }

    // Pass 2b: resolve import/call candidates.
    for fe in &per_file {
        for (src_id, target, kind) in &fe.candidates {
            if let Some(to) = resolve_target(target, src_id, &by_name, &id_path) {
                let _ = graph.insert_edge(KgEdge::new(src_id, &to, *kind, Confidence::Extracted));
            }
        }
    }

    graph.recompute_degrees();
    Ok(graph)
}

/// Resolve a candidate `target` (a bare name, or a one-level-qualified
/// `Qual::name`) to exactly one node id, or `None` (drop). Precedence:
/// 1. If qualified, a *unique* node whose id ends with `::Qual::name`;
///    a qualified target that matches >1 node is ambiguous → drop.
/// 2. Else a *unique* same-file node named `name`.
/// 3. Else a *unique* global node named `name`.
/// Every step demands uniqueness, so an ambiguous target is dropped rather than
/// mis-resolved to a wrong EXTRACTED edge. `src_id` is excluded (no self-edge).
fn resolve_target(
    target: &str,
    src_id: &str,
    by_name: &HashMap<String, Vec<String>>,
    id_path: &HashMap<String, String>,
) -> Option<String> {
    let simple = last_segment(target);
    let ids = by_name.get(simple)?;
    let pool: Vec<&String> = ids.iter().filter(|id| id.as_str() != src_id).collect();
    if pool.is_empty() {
        return None;
    }

    // 1) qualified preference
    if target.contains("::") {
        let want = format!("::{target}");
        let q: Vec<&String> = pool.iter().copied().filter(|id| id.ends_with(&want)).collect();
        match q.len() {
            1 => return Some(q[0].clone()),
            n if n > 1 => return None, // ambiguous even qualified
            _ => {}                    // no qualified hit → fall through to name rules
        }
    }

    // 2) same-file unique
    let src_path = id_path.get(src_id);
    let same: Vec<&String> = pool
        .iter()
        .copied()
        .filter(|id| id_path.get(id.as_str()) == src_path)
        .collect();
    match same.len() {
        1 => return Some(same[0].clone()),
        n if n > 1 => return None, // same-file ambiguous → drop
        _ => {}
    }

    // 3) global unique
    if pool.len() == 1 {
        Some(pool[0].clone())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_fqn_from_paths() {
        assert_eq!(module_fqn("src/scribe/graph/model.rs"), "crate::scribe::graph::model");
        assert_eq!(module_fqn("src/scribe/mod.rs"), "crate::scribe");
        assert_eq!(module_fqn("src/lib.rs"), "crate");
        assert_eq!(module_fqn("src/main.rs"), "crate");
    }

    #[test]
    fn emits_nodes_and_contains_edges() {
        let src = r#"
pub struct Widget { pub id: u64 }
pub fn build() -> Widget { Widget { id: 1 } }
impl Widget {
    pub fn rename(&mut self) -> bool { true }
}
pub enum Mode { Fast, Slow }
"#;
        let g = build_rust_graph("TERM", &[("src/w.rs".to_string(), src.to_string())]).unwrap();
        // module + Widget + build + rename + Mode
        assert!(g.get_node("crate::w").is_some(), "module node");
        assert!(g.get_node("crate::w::Widget").is_some(), "struct node");
        assert!(g.get_node("crate::w::build").is_some(), "fn node");
        assert!(g.get_node("crate::w::Mode").is_some(), "enum node");
        // impl method attributed to the type, not the module
        assert!(g.get_node("crate::w::Widget::rename").is_some(), "impl method node");
        // Contains: type -> method
        let has_contains = g.edges().any(|e| {
            e.from == "crate::w::Widget" && e.to == "crate::w::Widget::rename" && e.kind == EdgeKind::Contains
        });
        assert!(has_contains, "type Contains method edge");
    }

    #[test]
    fn resolves_same_file_call_edge_as_extracted() {
        let src = r#"
pub fn helper() -> u8 { 1 }
pub fn caller() -> u8 { helper() }
"#;
        let g = build_rust_graph("TERM", &[("src/c.rs".to_string(), src.to_string())]).unwrap();
        let call = g.edges().find(|e| e.kind == EdgeKind::Calls);
        let call = call.expect("a Calls edge");
        assert_eq!(call.from, "crate::c::caller");
        assert_eq!(call.to, "crate::c::helper");
        assert_eq!(call.confidence, Confidence::Extracted);
    }

    #[test]
    fn unresolved_call_is_dropped_not_fabricated() {
        let src = r#"
pub fn caller() { some_unknown_external_fn(); }
"#;
        let g = build_rust_graph("TERM", &[("src/u.rs".to_string(), src.to_string())]).unwrap();
        assert!(g.edges().all(|e| e.kind != EdgeKind::Calls), "no fabricated call edge");
    }

    #[test]
    fn malformed_file_is_skipped_batch_survives() {
        let bad = "pub fn oops( { { < <<< unclosed";
        let good = "pub fn healthy(x: u8) -> u8 { x }";
        let g = build_rust_graph(
            "TERM",
            &[
                ("src/bad.rs".to_string(), bad.to_string()),
                ("src/good.rs".to_string(), good.to_string()),
            ],
        )
        .unwrap();
        // The good file's symbol is still indexed even if the bad one is noise.
        assert!(g.get_node("crate::good::healthy").is_some(), "valid sibling indexed");
    }

    #[test]
    fn empty_input_is_empty_graph() {
        let g = build_rust_graph("TERM", &[]).unwrap();
        assert!(g.is_empty());
    }

    #[test]
    fn cross_file_import_resolves_to_unique_global() {
        let a = "pub struct Beacon { pub id: u64 }";
        let b = "use crate::a::Beacon;\npub fn use_it() {}";
        let g = build_rust_graph(
            "TERM",
            &[("src/a.rs".to_string(), a.to_string()), ("src/b.rs".to_string(), b.to_string())],
        )
        .unwrap();
        let imp = g
            .edges()
            .find(|e| e.kind == EdgeKind::Imports && e.to == "crate::a::Beacon");
        assert!(imp.is_some(), "import resolves to the unique global Beacon node");
    }

    #[test]
    fn generic_impl_method_attributes_to_base_type() {
        // Regression (review P2-1): `impl<T> Cache<T>` must FQN the method to
        // `crate::m::Cache::get`, and the type->method Contains edge must exist.
        let src = r#"
pub struct Cache<T> { items: Vec<T> }
impl<T> Cache<T> {
    pub fn get(&self) -> u8 { 0 }
}
"#;
        let g = build_rust_graph("TERM", &[("src/m.rs".to_string(), src.to_string())]).unwrap();
        assert!(g.get_node("crate::m::Cache").is_some(), "generic struct node");
        assert!(
            g.get_node("crate::m::Cache::get").is_some(),
            "method FQN strips generics"
        );
        assert!(
            g.edges().any(|e| e.from == "crate::m::Cache"
                && e.to == "crate::m::Cache::get"
                && e.kind == EdgeKind::Contains),
            "type Contains method edge survives for generic type"
        );
    }

    #[test]
    fn qualified_call_disambiguates_same_named_methods() {
        // Regression (review P2-2): two `new`s in one file; a qualified
        // `Foo::new()` must resolve to Foo::new, never Bar::new.
        let src = r#"
pub struct Foo;
pub struct Bar;
impl Foo { pub fn new() -> Foo { Foo } }
impl Bar { pub fn new() -> Bar { Bar } }
pub fn make() -> Foo { Foo::new() }
"#;
        let g = build_rust_graph("TERM", &[("src/m.rs".to_string(), src.to_string())]).unwrap();
        let call = g
            .edges()
            .find(|e| e.kind == EdgeKind::Calls && e.from == "crate::m::make");
        let call = call.expect("a Calls edge from make");
        assert_eq!(call.to, "crate::m::Foo::new", "qualified call must not mis-resolve to Bar::new");
    }

    #[test]
    fn ambiguous_unqualified_call_is_dropped_not_misresolved() {
        // Two `new`s; an UNqualified `new()` (no receiver) is ambiguous → drop,
        // never a wrong EXTRACTED edge.
        let src = r#"
pub struct Foo;
pub struct Bar;
impl Foo { pub fn new() -> Foo { Foo } }
impl Bar { pub fn new() -> Bar { Bar } }
pub fn make() -> Foo { new() }
"#;
        let g = build_rust_graph("TERM", &[("src/m.rs".to_string(), src.to_string())]).unwrap();
        assert!(
            !g.edges().any(|e| e.kind == EdgeKind::Calls && e.from == "crate::m::make"),
            "ambiguous unqualified call must be dropped, not mis-resolved"
        );
    }

    #[test]
    fn nested_item_in_body_is_not_emitted() {
        // Regression (review P3-3): a local item inside a fn body is not a node.
        let src = r#"
pub fn outer() -> u8 {
    struct Local { x: u8 }
    fn helper() -> u8 { 1 }
    helper()
}
"#;
        let g = build_rust_graph("TERM", &[("src/m.rs".to_string(), src.to_string())]).unwrap();
        assert!(g.get_node("crate::m::outer").is_some(), "top-level fn emitted");
        assert!(g.get_node("crate::m::outer::Local").is_none(), "local struct not a node");
        assert!(g.get_node("crate::m::outer::helper").is_none(), "local fn not a node");
    }

    #[test]
    fn use_as_alias_links_to_imported_symbol() {
        let a = "pub struct Beacon;";
        let b = "use crate::a::Beacon as B;\npub fn f() {}";
        let g = build_rust_graph(
            "TERM",
            &[("src/a.rs".to_string(), a.to_string()), ("src/b.rs".to_string(), b.to_string())],
        )
        .unwrap();
        assert!(
            g.edges().any(|e| e.kind == EdgeKind::Imports && e.to == "crate::a::Beacon"),
            "use-as links to the imported symbol, not the alias"
        );
    }
}
