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
/// Build a knowledge graph from a set of `(repo_relative_path, source)` files
/// of ANY supported language (KGRAPH-17). Each file is routed by extension: Rust
/// uses the precise Rust extractor; every other supported language uses the
/// generic tree-sitter extractor. Files whose language is unsupported are
/// skipped. Pass-2 resolution is shared.
pub fn build_graph(
    project_id: &str,
    files: &[(String, String)],
) -> Result<KnowledgeGraph, ToolError> {
    let mut graph = KnowledgeGraph::new(project_id);

    // Pass 1: nodes, dispatched per language.
    let per_file: Vec<FileExtract> = files
        .iter()
        .filter_map(|(p, s)| {
            Lang::from_path(p).map(|lang| match lang {
                Lang::Rust => extract_file(p, s),
                other => generic_extract_file(p, s, other),
            })
        })
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

    // Pass 2b: resolve IMPORTS first, recording a per-file `imported_name ->
    // definition id` map. This is the scope signal that lets a later call to a
    // name resolve to the symbol the file actually imported (KGRAPH-18), the
    // stack-graphs-free precision path.
    let mut file_imports: HashMap<String, HashMap<String, String>> = HashMap::new();
    for fe in &per_file {
        for (src_id, target, kind) in &fe.candidates {
            if *kind != EdgeKind::Imports {
                continue;
            }
            if let Some(to) = resolve_target(target, src_id, &by_name, &id_path) {
                let _ = graph.insert_edge(KgEdge::new(src_id, &to, *kind, Confidence::Extracted));
                if let Some(file) = id_path.get(src_id) {
                    file_imports
                        .entry(file.clone())
                        .or_default()
                        .insert(last_segment(target).to_string(), to);
                }
            }
        }
    }

    // Pass 2c: resolve CALLS / REFERENCES, preferring an import the SOURCE FILE
    // actually declares. A call to `N` from a file that imports `N` binds to the
    // imported definition even when `N` is ambiguous project-wide — turning a
    // previously-dropped ambiguous call into a precise edge. Falls back to the
    // uniqueness-checked resolver otherwise.
    for fe in &per_file {
        for (src_id, target, kind) in &fe.candidates {
            if *kind == EdgeKind::Imports {
                continue;
            }
            let simple = last_segment(target);
            let via_import = id_path
                .get(src_id)
                .and_then(|file| file_imports.get(file))
                .and_then(|m| m.get(simple))
                .filter(|to| to.as_str() != src_id)
                .cloned();
            let resolved = via_import.or_else(|| resolve_target(target, src_id, &by_name, &id_path));
            if let Some(to) = resolved {
                let _ = graph.insert_edge(KgEdge::new(src_id, &to, *kind, Confidence::Extracted));
            }
        }
    }

    graph.recompute_degrees();
    Ok(graph)
}

/// Back-compat alias: the Rust-specific name kept for existing callers/tests.
/// Now multi-language under the hood (routes each file by extension).
pub fn build_rust_graph(
    project_id: &str,
    files: &[(String, String)],
) -> Result<KnowledgeGraph, ToolError> {
    build_graph(project_id, files)
}

// ─── Multi-language extraction (KGRAPH-17) ───────────────────────────────────

/// Supported source languages. Rust has a dedicated precise extractor; the rest
/// share the generic tree-sitter extractor driven by a [`LangSpec`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Java,
    C,
    Cpp,
    Ruby,
    CSharp,
    Php,
    Bash,
    Lua,
}

impl Lang {
    pub fn from_path(path: &str) -> Option<Lang> {
        let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
        Some(match ext.as_str() {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            "go" => Lang::Go,
            "java" => Lang::Java,
            "c" | "h" => Lang::C,
            "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Lang::Cpp,
            "rb" => Lang::Ruby,
            "cs" => Lang::CSharp,
            "php" | "phtml" => Lang::Php,
            "sh" | "bash" => Lang::Bash,
            "lua" => Lang::Lua,
            _ => return None,
        })
    }

    fn ts_language(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Lang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Lang::Bash => tree_sitter_bash::LANGUAGE.into(),
            Lang::Lua => tree_sitter_lua::LANGUAGE.into(),
        }
    }

    /// The tree-sitter node kinds that define an emittable symbol, mapped to a
    /// graph [`NodeKind`]. A `Function` kind recurses as a call-scope; any other
    /// (type-like) kind recurses as a `Contains` container for its members.
    fn defs(self) -> &'static [(&'static str, NodeKind)] {
        use NodeKind::*;
        match self {
            Lang::Python => &[("function_definition", Function), ("class_definition", Class)],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &[
                ("function_declaration", Function),
                ("generator_function_declaration", Function),
                ("method_definition", Function),
                ("class_declaration", Class),
                ("abstract_class_declaration", Class),
                ("interface_declaration", Trait),
                ("type_alias_declaration", Struct),
                ("enum_declaration", Enum),
            ],
            Lang::Go => &[
                ("function_declaration", Function),
                ("method_declaration", Function),
                ("type_spec", Struct),
            ],
            Lang::Java => &[
                ("class_declaration", Class),
                ("interface_declaration", Trait),
                ("enum_declaration", Enum),
                ("record_declaration", Struct),
                ("method_declaration", Function),
                ("constructor_declaration", Function),
            ],
            Lang::C => &[
                ("function_definition", Function),
                ("struct_specifier", Struct),
                ("union_specifier", Struct),
                ("enum_specifier", Enum),
            ],
            Lang::Cpp => &[
                ("function_definition", Function),
                ("struct_specifier", Struct),
                ("union_specifier", Struct),
                ("class_specifier", Class),
                ("enum_specifier", Enum),
                ("namespace_definition", Module),
            ],
            Lang::Ruby => &[
                ("method", Function),
                ("singleton_method", Function),
                ("class", Class),
                ("module", Module),
            ],
            Lang::CSharp => &[
                ("class_declaration", Class),
                ("interface_declaration", Trait),
                ("struct_declaration", Struct),
                ("enum_declaration", Enum),
                ("method_declaration", Function),
                ("constructor_declaration", Function),
                ("namespace_declaration", Module),
            ],
            Lang::Php => &[
                ("function_definition", Function),
                ("method_declaration", Function),
                ("class_declaration", Class),
                ("interface_declaration", Trait),
                ("trait_declaration", Trait),
                ("enum_declaration", Enum),
            ],
            Lang::Bash => &[("function_definition", Function)],
            Lang::Lua => &[
                ("function_declaration", Function),
                ("function_definition", Function),
            ],
            Lang::Rust => &[], // handled by the dedicated extractor
        }
    }

    /// Call-expression node kinds (for `Calls` edges).
    fn calls(self) -> &'static [&'static str] {
        match self {
            Lang::Python | Lang::Lua => &["call", "function_call"],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx | Lang::Go | Lang::C | Lang::Cpp => &["call_expression"],
            Lang::Java => &["method_invocation", "object_creation_expression"],
            Lang::Ruby => &["call", "method_call"],
            Lang::CSharp => &["invocation_expression"],
            Lang::Php => &["function_call_expression", "member_call_expression", "scoped_call_expression"],
            Lang::Bash => &["command"],
            Lang::Rust => &[],
        }
    }

    /// Import/use node kinds (for `Imports` edges).
    fn imports(self) -> &'static [&'static str] {
        match self {
            Lang::Python => &["import_statement", "import_from_statement"],
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => &["import_statement"],
            Lang::Go => &["import_spec"],
            Lang::Java => &["import_declaration"],
            Lang::C | Lang::Cpp => &["preproc_include"],
            Lang::CSharp => &["using_directive"],
            Lang::Php => &["namespace_use_declaration"],
            Lang::Ruby | Lang::Bash | Lang::Lua | Lang::Rust => &[],
        }
    }

    fn def_kind(self, kind: &str) -> Option<NodeKind> {
        self.defs().iter().find(|(k, _)| *k == kind).map(|(_, nk)| *nk)
    }
}

/// Identifier-ish node kinds a generic name/callee/import lookup will accept.
const IDENT_KINDS: &[&str] = &[
    "identifier",
    "type_identifier",
    "field_identifier",
    "property_identifier",
    "constant",
    "name",
    "word",
    "scoped_identifier",
    "dotted_name",
    "namespace",
    "variable_name",
];

/// Derive a language-neutral module FQN from a repo-relative path, joining path
/// segments with `::` (used purely as a stable id separator). Drops a leading
/// `src`/`lib`/`app` dir and index-like file stems.
fn generic_module_fqn(path: &str) -> String {
    let p = path.trim_start_matches("./");
    let stem = match p.rsplit_once('.') {
        Some((s, _)) => s,
        None => p,
    };
    let mut parts: Vec<&str> = stem.split('/').filter(|s| !s.is_empty()).collect();
    if matches!(parts.first().copied(), Some("src") | Some("lib") | Some("app")) {
        parts.remove(0);
    }
    if matches!(
        parts.last().copied(),
        Some("mod") | Some("lib") | Some("main") | Some("index") | Some("__init__") | Some("init")
    ) {
        parts.pop();
    }
    if parts.is_empty() {
        return "root".to_string();
    }
    parts.join("::")
}

/// Find the first identifier-ish descendant text (BFS to `max_depth`).
fn first_ident(node: Node, src: &[u8], max_depth: usize) -> Option<String> {
    let mut q: std::collections::VecDeque<(Node, usize)> = std::collections::VecDeque::new();
    q.push_back((node, 0));
    while let Some((n, d)) = q.pop_front() {
        if d > 0 && IDENT_KINDS.contains(&n.kind()) {
            if let Ok(t) = n.utf8_text(src) {
                let t = t.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
        }
        if d < max_depth {
            let mut c = n.walk();
            for ch in n.named_children(&mut c) {
                q.push_back((ch, d + 1));
            }
        }
    }
    None
}

/// The defined symbol's short name: the `name` field if present, else the first
/// identifier-ish descendant (handles C-style declarators, etc.).
fn generic_name_of(node: Node, src: &[u8]) -> Option<String> {
    if let Some(nf) = node.child_by_field_name("name") {
        if let Ok(t) = nf.utf8_text(src) {
            let t = last_segment(t.trim());
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    first_ident(node, src, 3).map(|s| last_segment(&s).to_string())
}

/// The callee short name of a call node.
fn generic_callee(node: Node, src: &[u8]) -> Option<String> {
    for field in ["function", "name", "method"] {
        if let Some(f) = node.child_by_field_name(field) {
            // a field may itself be scoped/attribute — take its last identifier
            if let Ok(t) = f.utf8_text(src) {
                let seg = last_segment(t.trim());
                if !seg.is_empty() {
                    return Some(seg.to_string());
                }
            }
        }
    }
    first_ident(node, src, 2)
}

/// Imported symbols from an import node, QUALIFIED where the import names a
/// source module (`from a.b import foo` -> `b::foo`; JS `import {foo} from
/// "x/y"` -> `y::foo`). The qualifier lets the resolver bind a call to the
/// specific imported definition even when the bare name is ambiguous project-
/// wide (KGRAPH-18). Falls back to bare last-segment names when no module
/// qualifier is discernible.
fn generic_imports(node: Node, src: &[u8]) -> Vec<String> {
    // The module qualifier: python `module_name` field, else a string source
    // (JS/TS `import ... from "path"`), else none.
    let qualifier: Option<String> = node
        .child_by_field_name("module_name")
        .and_then(|m| m.utf8_text(src).ok())
        .map(|t| last_segment(t.trim()).to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            // a string literal source anywhere in the import (JS/TS)
            let mut c = node.walk();
            let strnode = node
                .named_children(&mut c)
                .find(|ch| ch.kind() == "string" || ch.kind() == "string_fragment");
            let seg = strnode
                .and_then(|s| s.utf8_text(src).ok())
                .map(|t| last_segment(t.trim_matches(|c| c == '"' || c == '\'' || c == '/')).to_string())
                .filter(|s| !s.is_empty());
            seg
        });

    let mut names = Vec::new();
    let module_name_node = node.child_by_field_name("module_name");
    let mut q: std::collections::VecDeque<Node> = std::collections::VecDeque::new();
    q.push_back(node);
    while let Some(n) = q.pop_front() {
        // skip the module-path subtree so its segments aren't taken as imported names
        if Some(n) == module_name_node {
            continue;
        }
        if IDENT_KINDS.contains(&n.kind()) {
            if let Ok(t) = n.utf8_text(src) {
                let seg = last_segment(t.trim());
                if !seg.is_empty() && seg != "self" {
                    names.push(seg.to_string());
                }
            }
        }
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            q.push_back(ch);
        }
    }
    names.dedup();
    let mut out: Vec<String> = match &qualifier {
        Some(qual) => names.into_iter().map(|nm| format!("{qual}::{nm}")).collect(),
        None => names,
    };
    out.truncate(16);
    out
}

/// Generic recursive walk for a non-Rust language, mirroring the Rust walk but
/// driven by the language's [`LangSpec`].
fn generic_walk(ctx: &mut Ctx, node: Node, container_id: &str, enclosing_fn: Option<&str>, lang: Lang) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let kind = child.kind();

        if lang.imports().contains(&kind) {
            for name in generic_imports(child, ctx.src) {
                ctx.out.candidates.push((ctx.module.clone(), name, EdgeKind::Imports));
            }
            continue;
        }
        if lang.calls().contains(&kind) {
            if let Some(src_id) = enclosing_fn {
                if let Some(callee) = generic_callee(child, ctx.src) {
                    ctx.out.candidates.push((src_id.to_string(), callee, EdgeKind::Calls));
                }
            }
            generic_walk(ctx, child, container_id, enclosing_fn, lang);
            continue;
        }
        if let Some(nk) = lang.def_kind(kind) {
            // Inside a function body → a local/nested def; not a node. Recurse
            // for calls only (keeps top-level items + methods, like the Rust path).
            if enclosing_fn.is_some() {
                generic_walk(ctx, child, container_id, enclosing_fn, lang);
                continue;
            }
            let Some(name) = generic_name_of(child, ctx.src) else {
                generic_walk(ctx, child, container_id, enclosing_fn, lang);
                continue;
            };
            let id = format!("{container_id}::{name}");
            ctx.push_node(&id, nk, &name, child);
            ctx.out.contains.push((container_id.to_string(), id.clone()));
            if nk == NodeKind::Function {
                generic_walk(ctx, child, &id, Some(&id), lang);
            } else {
                // type-like: its members are Contained by it
                generic_walk(ctx, child, &id, enclosing_fn, lang);
            }
            continue;
        }
        generic_walk(ctx, child, container_id, enclosing_fn, lang);
    }
}

/// Extract nodes + edge candidates from one non-Rust file.
fn generic_extract_file(path: &str, source: &str, lang: Lang) -> FileExtract {
    let module = generic_module_fqn(path);
    let out = FileExtract {
        nodes: vec![KgNode::new(&module, NodeKind::Module, last_segment(&module), path)],
        contains: Vec::new(),
        candidates: Vec::new(),
    };
    let mut parser = Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
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
    generic_walk(&mut ctx, tree.root_node(), &module, None, lang);
    ctx.out
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
    fn python_class_method_and_call() {
        let src = "def helper():\n    pass\n\ndef caller():\n    helper()\n\nclass Widget:\n    def rename(self):\n        pass\n";
        let g = build_graph("P", &[("pkg/w.py".to_string(), src.to_string())]).unwrap();
        assert!(g.get_node("pkg::w::helper").is_some(), "fn");
        assert!(g.get_node("pkg::w::Widget").is_some(), "class");
        assert!(g.get_node("pkg::w::Widget::rename").is_some(), "method attributed to class");
        assert!(g.edges().any(|e| e.from == "pkg::w::Widget" && e.to == "pkg::w::Widget::rename" && e.kind == EdgeKind::Contains));
        assert!(g.edges().any(|e| e.from == "pkg::w::caller" && e.to == "pkg::w::helper" && e.kind == EdgeKind::Calls));
    }

    #[test]
    fn go_functions_and_call() {
        let src = "package p\nfunc Foo() { Bar() }\nfunc Bar() {}\n";
        let g = build_graph("G", &[("svc/x.go".to_string(), src.to_string())]).unwrap();
        assert!(g.get_node("svc::x::Foo").is_some());
        assert!(g.get_node("svc::x::Bar").is_some());
        assert!(g.edges().any(|e| e.from == "svc::x::Foo" && e.to == "svc::x::Bar" && e.kind == EdgeKind::Calls));
    }

    #[test]
    fn lua_function_extracted_unblocks_civic_rail() {
        let src = "function greet(name)\n  print('hi '..name)\nend\n\nlocal function helper()\nend\n";
        let g = build_graph("RAIL", &[("cfg/a.lua".to_string(), src.to_string())]).unwrap();
        assert!(g.get_node("cfg::a::greet").is_some(), "lua function extracted");
        assert!(g.node_count() >= 2, "module + at least one function");
    }

    #[test]
    fn java_class_method_contains_and_call() {
        let src = "class Foo {\n  void m() { n(); }\n  void n() {}\n}\n";
        let g = build_graph("J", &[("com/widgets.java".to_string(), src.to_string())]).unwrap();
        assert!(g.get_node("com::widgets::Foo").is_some(), "class");
        assert!(g.get_node("com::widgets::Foo::m").is_some(), "method");
        assert!(g.edges().any(|e| e.from == "com::widgets::Foo" && e.to == "com::widgets::Foo::m" && e.kind == EdgeKind::Contains));
        assert!(g.edges().any(|e| e.from == "com::widgets::Foo::m" && e.to == "com::widgets::Foo::n" && e.kind == EdgeKind::Calls));
    }

    #[test]
    fn javascript_functions_and_call() {
        let src = "function f() { g(); }\nfunction g() {}\n";
        let g = build_graph("W", &[("web/app.js".to_string(), src.to_string())]).unwrap();
        assert!(g.get_node("web::app::f").is_some());
        assert!(g.edges().any(|e| e.from == "web::app::f" && e.to == "web::app::g" && e.kind == EdgeKind::Calls));
    }

    #[test]
    fn ruby_class_and_method() {
        let src = "class Foo\n  def bar\n  end\nend\n";
        let g = build_graph("R", &[("lib/foo.rb".to_string(), src.to_string())]).unwrap();
        // lib/ is stripped -> module "foo"
        assert!(g.get_node("foo::Foo").is_some(), "ruby class");
        assert!(g.get_node("foo::Foo::bar").is_some(), "ruby method");
    }

    #[test]
    fn mixed_language_project_builds_all() {
        let files = vec![
            ("a.py".to_string(), "def pyf():\n    pass\n".to_string()),
            ("b.lua".to_string(), "function luaf()\nend\n".to_string()),
            ("c.go".to_string(), "package c\nfunc Gof() {}\n".to_string()),
            ("d.rs".to_string(), "pub fn rustf() {}\n".to_string()),
            ("readme.txt".to_string(), "not code".to_string()), // unsupported → skipped
        ];
        let g = build_graph("MIX", &files).unwrap();
        assert!(g.get_node("a::pyf").is_some(), "python");
        assert!(g.get_node("b::luaf").is_some(), "lua");
        assert!(g.get_node("c::Gof").is_some(), "go");
        assert!(g.get_node("crate::d::rustf").is_some(), "rust keeps crate:: FQN");
    }

    #[test]
    fn import_aware_resolution_disambiguates_cross_file_call() {
        // Two files define `helper`; c imports a's and calls it. Without import
        // awareness the call is globally ambiguous and dropped; with it, the
        // call binds to a's helper (KGRAPH-18).
        let files = vec![
            ("pkg/a.py".to_string(), "def helper():\n    return 1\n".to_string()),
            ("pkg/b.py".to_string(), "def helper():\n    return 2\n".to_string()),
            ("pkg/c.py".to_string(), "from a import helper\n\ndef use():\n    return helper()\n".to_string()),
        ];
        let g = build_graph("P", &files).unwrap();
        let call = g
            .edges()
            .find(|e| e.kind == EdgeKind::Calls && e.from == "pkg::c::use")
            .expect("call resolved via the import (would be dropped as ambiguous otherwise)");
        assert_eq!(call.to, "pkg::a::helper", "bound to the imported definition, not b's");
        assert!(
            g.edges().any(|e| e.kind == EdgeKind::Imports && e.from == "pkg::c" && e.to == "pkg::a::helper"),
            "qualified import edge present"
        );
    }

    #[test]
    fn lang_from_path_covers_the_set() {
        assert_eq!(Lang::from_path("x.py"), Some(Lang::Python));
        assert_eq!(Lang::from_path("x.lua"), Some(Lang::Lua));
        assert_eq!(Lang::from_path("x.go"), Some(Lang::Go));
        assert_eq!(Lang::from_path("x.tsx"), Some(Lang::Tsx));
        assert_eq!(Lang::from_path("x.cpp"), Some(Lang::Cpp));
        assert_eq!(Lang::from_path("x.rb"), Some(Lang::Ruby));
        assert_eq!(Lang::from_path("x.md"), None);
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
