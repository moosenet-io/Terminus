//! Atlas visual renderers (KGRAPH-08): SVG, GraphML, and a self-contained HTML
//! page — all projections of the SAME [`LayoutResult`] (KGRAPH-07), so the
//! picture is identical everywhere.
//!
//! The SVG is the artifact Scribe embeds in generated docs (KGRAPH-09): nodes
//! are circles colored by `cluster` and sized by `degree`; edges are drawn with
//! a `stroke-dasharray` keyed to their confidence tier — **solid** EXTRACTED,
//! **dashed** INFERRED, **dotted** AMBIGUOUS — with a legend. A very large graph
//! is capped to the top-degree nodes with a *visible* caption (never a silent
//! truncation).
//!
//! Pure string building: no I/O, networking, secrets, or external assets — the
//! HTML references no external hosts (CSP-safe), using inline SVG plus a small
//! vanilla-JS pan/zoom/search (not a CDN library).

use super::layout::{cluster_hue, node_radius, LayoutResult};
use super::model::{Confidence, KnowledgeGraph};

/// Above this node count the SVG/HTML render the top-degree nodes only and show
/// a caption saying how many were dropped (visible, not silent).
const MAX_RENDER_NODES: usize = 1200;

/// XML/HTML text escape for attribute and body text.
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// SVG `stroke-dasharray` per confidence: solid / dashed / dotted.
fn dash(c: Confidence) -> &'static str {
    match c {
        Confidence::Extracted => "",
        Confidence::Inferred => "7,4",
        Confidence::Ambiguous => "2,4",
    }
}

/// Choose the nodes to render: all, or (if over the cap) the top-degree slice.
/// Returns (chosen ids as a set-like sorted Vec, dropped count).
fn chosen<'a>(graph: &'a KnowledgeGraph) -> (Vec<&'a str>, usize) {
    let total = graph.node_count();
    if total <= MAX_RENDER_NODES {
        let mut ids: Vec<&str> = graph.nodes().map(|n| n.id.as_str()).collect();
        ids.sort_unstable();
        (ids, 0)
    } else {
        let mut by_deg: Vec<&super::model::KgNode> = graph.nodes().collect();
        by_deg.sort_by(|a, b| b.degree.cmp(&a.degree).then(a.id.cmp(&b.id)));
        let mut ids: Vec<&str> = by_deg.iter().take(MAX_RENDER_NODES).map(|n| n.id.as_str()).collect();
        ids.sort_unstable();
        (ids, total - MAX_RENDER_NODES)
    }
}

/// Render the laid-out graph to a standalone SVG document.
pub fn to_svg(graph: &KnowledgeGraph, layout: &LayoutResult) -> String {
    let (ids, dropped) = chosen(graph);
    let shown: std::collections::HashSet<&str> = ids.iter().copied().collect();
    let (w, h) = (layout.width, layout.height);

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {w} {h}\" width=\"{w}\" height=\"{h}\" font-family=\"system-ui, sans-serif\">\n"
    ));
    s.push_str("<rect width=\"100%\" height=\"100%\" fill=\"#ffffff\"/>\n");

    // Edges first (drawn under nodes). Only among shown nodes.
    s.push_str("<g stroke-linecap=\"round\">\n");
    for e in graph.edges() {
        if !shown.contains(e.from.as_str()) || !shown.contains(e.to.as_str()) {
            continue;
        }
        let (Some(&(x1, y1)), Some(&(x2, y2))) =
            (layout.positions.get(&e.from), layout.positions.get(&e.to))
        else {
            continue;
        };
        let da = dash(e.confidence);
        let da_attr = if da.is_empty() { String::new() } else { format!(" stroke-dasharray=\"{da}\"") };
        s.push_str(&format!(
            "<line x1=\"{x1:.1}\" y1=\"{y1:.1}\" x2=\"{x2:.1}\" y2=\"{y2:.1}\" stroke=\"#b6bcc6\" stroke-width=\"1\"{da_attr}/>\n"
        ));
    }
    s.push_str("</g>\n");

    // Nodes.
    s.push_str("<g stroke=\"#ffffff\" stroke-width=\"1\">\n");
    for id in &ids {
        let Some(n) = graph.get_node(id) else { continue };
        let Some(&(x, y)) = layout.positions.get(*id) else { continue };
        let r = node_radius(n.degree);
        let fill = cluster_hue(n.cluster);
        s.push_str(&format!(
            "<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"{r:.1}\" fill=\"{fill}\"><title>{}</title></circle>\n",
            esc(&n.id)
        ));
    }
    s.push_str("</g>\n");

    // Legend.
    s.push_str(&legend_svg(h));

    if dropped > 0 {
        s.push_str(&format!(
            "<text x=\"12\" y=\"20\" font-size=\"13\" fill=\"#5f6673\">showing top {MAX_RENDER_NODES} of {} nodes ({dropped} not shown)</text>\n",
            graph.node_count()
        ));
    }

    s.push_str("</svg>\n");
    s
}

fn legend_svg(h: f32) -> String {
    let y = h - 78.0;
    let mut s = String::new();
    s.push_str(&format!("<g transform=\"translate(12,{y:.0})\" font-size=\"12\" fill=\"#3b4148\">\n"));
    s.push_str("<rect x=\"-4\" y=\"-16\" width=\"210\" height=\"78\" fill=\"#ffffff\" fill-opacity=\"0.85\" stroke=\"#e0e4ea\" rx=\"6\"/>\n");
    s.push_str("<text x=\"0\" y=\"0\" font-weight=\"600\">Edge confidence</text>\n");
    s.push_str("<line x1=\"0\" y1=\"16\" x2=\"36\" y2=\"16\" stroke=\"#8a929e\" stroke-width=\"2\"/><text x=\"44\" y=\"20\">EXTRACTED</text>\n");
    s.push_str("<line x1=\"0\" y1=\"32\" x2=\"36\" y2=\"32\" stroke=\"#8a929e\" stroke-width=\"2\" stroke-dasharray=\"7,4\"/><text x=\"44\" y=\"36\">INFERRED</text>\n");
    s.push_str("<line x1=\"0\" y1=\"48\" x2=\"36\" y2=\"48\" stroke=\"#8a929e\" stroke-width=\"2\" stroke-dasharray=\"2,4\"/><text x=\"44\" y=\"52\">AMBIGUOUS</text>\n");
    s.push_str("</g>\n");
    s
}

/// Render the graph to a GraphML document (Gephi/yEd/Cytoscape interchange).
pub fn to_graphml(graph: &KnowledgeGraph) -> String {
    let mut s = String::new();
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<graphml xmlns=\"http://graphml.graphdrawing.org/xmlns\">\n");
    s.push_str("<key id=\"d_kind\" for=\"node\" attr.name=\"kind\" attr.type=\"string\"/>\n");
    s.push_str("<key id=\"d_name\" for=\"node\" attr.name=\"name\" attr.type=\"string\"/>\n");
    s.push_str("<key id=\"d_cluster\" for=\"node\" attr.name=\"cluster\" attr.type=\"long\"/>\n");
    s.push_str("<key id=\"d_degree\" for=\"node\" attr.name=\"degree\" attr.type=\"long\"/>\n");
    s.push_str("<key id=\"e_kind\" for=\"edge\" attr.name=\"kind\" attr.type=\"string\"/>\n");
    s.push_str("<key id=\"e_conf\" for=\"edge\" attr.name=\"confidence\" attr.type=\"string\"/>\n");
    s.push_str("<graph edgedefault=\"directed\">\n");
    for n in graph.nodes() {
        s.push_str(&format!("<node id=\"{}\">", esc(&n.id)));
        s.push_str(&format!("<data key=\"d_kind\">{}</data>", n.kind.as_str()));
        s.push_str(&format!("<data key=\"d_name\">{}</data>", esc(&n.name)));
        if let Some(c) = n.cluster {
            s.push_str(&format!("<data key=\"d_cluster\">{c}</data>"));
        }
        s.push_str(&format!("<data key=\"d_degree\">{}</data>", n.degree));
        s.push_str("</node>\n");
    }
    for e in graph.edges() {
        s.push_str(&format!("<edge source=\"{}\" target=\"{}\">", esc(&e.from), esc(&e.to)));
        s.push_str(&format!("<data key=\"e_kind\">{}</data>", e.kind.as_str()));
        s.push_str(&format!("<data key=\"e_conf\">{}</data>", e.confidence.as_str()));
        s.push_str("</edge>\n");
    }
    s.push_str("</graph>\n</graphml>\n");
    s
}

/// Render a self-contained, interactive HTML page (inline SVG + a small
/// vanilla-JS pan/zoom/search). References NO external hosts — CSP-safe.
pub fn to_html(graph: &KnowledgeGraph, layout: &LayoutResult) -> String {
    let svg = to_svg(graph, layout);
    let project = esc(&graph.project_id);
    // The inline SVG gets an id so the JS can transform it. Insert id into the
    // opening <svg ...> tag.
    let svg = svg.replacen("<svg ", "<svg id=\"kg\" ", 1);
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"/>
<meta name="viewport" content="width=device-width, initial-scale=1"/>
<title>Atlas — {project} knowledge graph</title>
<style>
  body {{ margin:0; font-family:system-ui,sans-serif; background:#f5f7fa; color:#171b22; }}
  header {{ padding:10px 14px; border-bottom:1px solid #e0e4ea; display:flex; gap:12px; align-items:center; }}
  header b {{ font-size:14px; }}
  #q {{ padding:5px 9px; border:1px solid #cfd4dc; border-radius:6px; font:inherit; }}
  #wrap {{ overflow:hidden; height:calc(100vh - 46px); cursor:grab; }}
  #wrap:active {{ cursor:grabbing; }}
  #kg {{ transform-origin:0 0; max-width:none; }}
  .dim circle {{ opacity:0.12; }}
</style></head>
<body>
<header><b>Atlas · {project}</b><input id="q" placeholder="highlight nodes by id…"/>
<span style="color:#6b7482;font-size:12px">scroll to zoom · drag to pan</span></header>
<div id="wrap">{svg}</div>
<script>
(function(){{
  var wrap=document.getElementById('wrap'), svg=document.getElementById('kg');
  var scale=1, tx=0, ty=0, dragging=false, sx=0, sy=0;
  function apply(){{ svg.style.transform='translate('+tx+'px,'+ty+'px) scale('+scale+')'; }}
  wrap.addEventListener('wheel',function(e){{ e.preventDefault();
    var f=e.deltaY<0?1.1:0.9; scale=Math.max(0.1,Math.min(8,scale*f)); apply(); }},{{passive:false}});
  wrap.addEventListener('mousedown',function(e){{ dragging=true; sx=e.clientX-tx; sy=e.clientY-ty; }});
  window.addEventListener('mouseup',function(){{ dragging=false; }});
  window.addEventListener('mousemove',function(e){{ if(!dragging)return; tx=e.clientX-sx; ty=e.clientY-sy; apply(); }});
  var q=document.getElementById('q');
  q.addEventListener('input',function(){{
    var v=q.value.toLowerCase();
    svg.querySelectorAll('circle').forEach(function(c){{
      var t=c.querySelector('title'); var id=t?t.textContent.toLowerCase():'';
      c.parentNode.classList.toggle('dim', v && id.indexOf(v)<0);
    }});
  }});
}})();
</script>
</body></html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::layout::layout;
    use super::super::model::{Confidence, EdgeKind, KgEdge, KgNode, NodeKind};

    fn sample() -> (KnowledgeGraph, LayoutResult) {
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(KgNode::new("crate::a::foo", NodeKind::Function, "foo", "src/a.rs"));
        g.insert_node(KgNode::new("crate::b::Bar", NodeKind::Struct, "Bar", "src/b.rs"));
        g.insert_node(KgNode::new("crate::c::baz", NodeKind::Function, "baz", "src/c.rs"));
        g.insert_edge(KgEdge::new("crate::a::foo", "crate::b::Bar", EdgeKind::Calls, Confidence::Extracted)).unwrap();
        g.insert_edge(KgEdge::new("crate::c::baz", "crate::b::Bar", EdgeKind::References, Confidence::Inferred)).unwrap();
        g.recompute_degrees();
        let l = layout(&g);
        (g, l)
    }

    #[test]
    fn svg_has_a_circle_per_node_and_confidence_dashes() {
        let (g, l) = sample();
        let svg = to_svg(&g, &l);
        assert_eq!(svg.matches("<circle").count(), 3, "one circle per node");
        // an INFERRED edge => a dashed line with 7,4 dasharray somewhere
        assert!(svg.contains("stroke-dasharray=\"7,4\""), "inferred edge dashed");
        assert!(svg.contains("Edge confidence"), "legend present");
        assert!(svg.trim_start().starts_with("<svg"));
    }

    #[test]
    fn special_chars_in_id_are_escaped() {
        let mut g = KnowledgeGraph::new("TERM");
        g.insert_node(KgNode::new("crate::a::<weird & \"name\">", NodeKind::Function, "w", "src/a.rs"));
        let l = layout(&g);
        let svg = to_svg(&g, &l);
        assert!(svg.contains("&lt;weird &amp;"), "special chars escaped in title");
        assert!(!svg.contains("<weird"), "no raw < from node id leaks into markup");
    }

    #[test]
    fn graphml_has_node_and_edge_per_element() {
        let (g, _) = sample();
        let x = to_graphml(&g);
        assert!(x.contains("<graphml"));
        assert_eq!(x.matches("<node ").count(), 3);
        assert_eq!(x.matches("<edge ").count(), 2);
        assert!(x.contains("<data key=\"e_conf\">inferred</data>"));
    }

    #[test]
    fn html_references_no_external_hosts() {
        let (g, l) = sample();
        let html = to_html(&g, &l);
        // The only "http" allowed is the SVG xmlns namespace; assert no external
        // resource actually loads (script/link/img src/href or CDN).
        assert!(!html.contains("src=\"http"), "no external script src");
        assert!(!html.contains("href=\"http"), "no external stylesheet/link");
        assert!(!html.contains("cdn"), "no CDN reference");
        assert!(html.contains("<svg id=\"kg\""), "inline svg embedded");
    }

    #[test]
    fn empty_graph_renders_without_panic() {
        let g = KnowledgeGraph::new("TERM");
        let l = layout(&g);
        let svg = to_svg(&g, &l);
        assert!(svg.contains("</svg>"));
        assert_eq!(svg.matches("<circle").count(), 0);
        let x = to_graphml(&g);
        assert!(x.contains("</graphml>"));
        let h = to_html(&g, &l);
        assert!(h.contains("</html>"));
    }
}
