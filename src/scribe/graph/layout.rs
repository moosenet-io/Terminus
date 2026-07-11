//! Atlas shared force-directed layout (KGRAPH-07).
//!
//! Computes ONE set of 2-D node coordinates from the graph, used by every
//! visual output (SVG + HTML in KGRAPH-08) so the picture is consistent
//! everywhere: position from the layout (connected entities sit close, so
//! proximity in the picture is real coupling), color from the Leiden/Louvain
//! `cluster`, size from `degree`.
//!
//! The layout is a Fruchterman–Reingold spring embedding, **seeded
//! deterministically** (initial positions on a circle in sorted-id order, fixed
//! iteration count and order, no RNG), so identical input always yields
//! identical coordinates — reproducible docs and snapshot tests. Pure
//! computation: no I/O, networking, or secrets.

use std::collections::BTreeMap;

use super::model::KnowledgeGraph;

/// Laid-out coordinates plus the bounds every renderer draws within.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutResult {
    /// node id -> (x, y) inside `[0, width] x [0, height]`.
    pub positions: BTreeMap<String, (f32, f32)>,
    pub width: f32,
    pub height: f32,
}

const W: f32 = 1000.0;
const H: f32 = 1000.0;
const MARGIN: f32 = 40.0;

/// Compute a deterministic force-directed layout for the graph.
pub fn layout(graph: &KnowledgeGraph) -> LayoutResult {
    // Sort explicitly so determinism is self-contained and does not silently
    // depend on the model's internal node storage order (it is a BTreeMap today,
    // but this makes the "sorted-id order" contract independent of that).
    let mut ids: Vec<&str> = graph.nodes().map(|n| n.id.as_str()).collect();
    ids.sort_unstable();
    let n = ids.len();
    let mut positions = BTreeMap::new();

    if n == 0 {
        return LayoutResult { positions, width: W, height: H };
    }
    if n == 1 {
        positions.insert(ids[0].to_string(), (W / 2.0, H / 2.0));
        return LayoutResult { positions, width: W, height: H };
    }

    let index: BTreeMap<&str, usize> = ids.iter().enumerate().map(|(i, s)| (*s, i)).collect();

    // Deterministic initial placement: evenly on a circle in sorted-id order.
    let cx = W / 2.0;
    let cy = H / 2.0;
    let r0 = (W.min(H) / 2.0) - MARGIN;
    let mut pos: Vec<(f32, f32)> = (0..n)
        .map(|i| {
            let theta = std::f32::consts::TAU * (i as f32) / (n as f32);
            (cx + r0 * theta.cos(), cy + r0 * theta.sin())
        })
        .collect();

    // Edge list as index pairs (all edge kinds attract — Contains pulls a method
    // toward its type, which is desirable for the picture).
    let edges: Vec<(usize, usize)> = graph
        .edges()
        .filter_map(|e| match (index.get(e.from.as_str()), index.get(e.to.as_str())) {
            (Some(&a), Some(&b)) if a != b => Some((a, b)),
            _ => None,
        })
        .collect();

    // Fruchterman–Reingold.
    let area = W * H;
    let k = 0.9 * (area / n as f32).sqrt(); // ideal edge length
    let iters = if n <= 400 { 90 } else { 40 };
    let mut temp = W / 10.0;
    let cool = temp / (iters as f32 + 1.0);

    let mut disp = vec![(0.0f32, 0.0f32); n];
    for _ in 0..iters {
        for d in disp.iter_mut() {
            *d = (0.0, 0.0);
        }
        // Repulsion between all pairs (deterministic order).
        for i in 0..n {
            for j in (i + 1)..n {
                let dx = pos[i].0 - pos[j].0;
                let dy = pos[i].1 - pos[j].1;
                let dist = (dx * dx + dy * dy).sqrt().max(0.01);
                let force = k * k / dist;
                let ux = dx / dist;
                let uy = dy / dist;
                disp[i].0 += ux * force;
                disp[i].1 += uy * force;
                disp[j].0 -= ux * force;
                disp[j].1 -= uy * force;
            }
        }
        // Attraction along edges.
        for &(a, b) in &edges {
            let dx = pos[a].0 - pos[b].0;
            let dy = pos[a].1 - pos[b].1;
            let dist = (dx * dx + dy * dy).sqrt().max(0.01);
            let force = dist * dist / k;
            let ux = dx / dist;
            let uy = dy / dist;
            disp[a].0 -= ux * force;
            disp[a].1 -= uy * force;
            disp[b].0 += ux * force;
            disp[b].1 += uy * force;
        }
        // Apply, capped by temperature; keep inside bounds.
        for i in 0..n {
            let dlen = (disp[i].0 * disp[i].0 + disp[i].1 * disp[i].1).sqrt().max(0.01);
            let step = dlen.min(temp);
            pos[i].0 += disp[i].0 / dlen * step;
            pos[i].1 += disp[i].1 / dlen * step;
            pos[i].0 = pos[i].0.clamp(0.0, W);
            pos[i].1 = pos[i].1.clamp(0.0, H);
        }
        temp = (temp - cool).max(0.0);
    }

    // Normalize the final cloud to fill the drawable area (margins on all sides).
    let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in &pos {
        minx = minx.min(x);
        miny = miny.min(y);
        maxx = maxx.max(x);
        maxy = maxy.max(y);
    }
    let spanx = (maxx - minx).max(1e-3);
    let spany = (maxy - miny).max(1e-3);
    let draw_x = W - 2.0 * MARGIN;
    let draw_y = H - 2.0 * MARGIN;
    for (i, id) in ids.iter().enumerate() {
        let nx = MARGIN + (pos[i].0 - minx) / spanx * draw_x;
        let ny = MARGIN + (pos[i].1 - miny) / spany * draw_y;
        positions.insert((*id).to_string(), (nx, ny));
    }

    LayoutResult { positions, width: W, height: H }
}

/// A stable, colorblind-conscious 12-hue palette; cluster id cycles through it.
/// `None` (unclustered) gets a neutral grey. Returned as a `#rrggbb` string the
/// SVG/HTML renderers embed directly.
pub fn cluster_hue(cluster: Option<u32>) -> &'static str {
    const PALETTE: [&str; 12] = [
        "#4E79A7", "#F28E2B", "#59A14F", "#E15759", "#B07AA1", "#76B7B2",
        "#EDC948", "#FF9DA7", "#9C755F", "#BAB0AC", "#86BCB6", "#D37295",
    ];
    match cluster {
        Some(c) => PALETTE[(c as usize) % PALETTE.len()],
        None => "#9AA0A6",
    }
}

/// Node radius from degree — sqrt scaling so hubs read larger without dwarfing
/// leaves. Bounded to a sane range for the fixed 1000-unit viewBox.
pub fn node_radius(degree: u32) -> f32 {
    (5.0 + 2.2 * (degree as f32).sqrt()).min(28.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::model::{Confidence, EdgeKind, KgEdge, KgNode, NodeKind};

    fn node(id: &str) -> KgNode {
        KgNode::new(id, NodeKind::Function, id, "src/x.rs")
    }

    fn graph(ids: &[&str], edges: &[(&str, &str)]) -> KnowledgeGraph {
        let mut g = KnowledgeGraph::new("TERM");
        for id in ids {
            g.insert_node(node(id));
        }
        for (a, b) in edges {
            g.insert_edge(KgEdge::new(*a, *b, EdgeKind::Calls, Confidence::Extracted)).unwrap();
        }
        g
    }

    #[test]
    fn deterministic_across_runs() {
        let g = graph(&["a", "b", "c", "d"], &[("a", "b"), ("b", "c"), ("c", "d")]);
        assert_eq!(layout(&g), layout(&g), "layout is deterministic");
    }

    #[test]
    fn all_positions_within_bounds() {
        let g = graph(&["a", "b", "c", "d", "e"], &[("a", "b"), ("a", "c"), ("d", "e")]);
        let l = layout(&g);
        assert_eq!(l.positions.len(), 5);
        for (_, &(x, y)) in &l.positions {
            assert!(x >= 0.0 && x <= l.width, "x in bounds: {x}");
            assert!(y >= 0.0 && y <= l.height, "y in bounds: {y}");
        }
    }

    #[test]
    fn empty_and_single_node_do_not_panic() {
        let empty = KnowledgeGraph::new("TERM");
        let l = layout(&empty);
        assert!(l.positions.is_empty());

        let mut solo = KnowledgeGraph::new("TERM");
        solo.insert_node(node("only"));
        let l = layout(&solo);
        assert_eq!(l.positions.get("only"), Some(&(W / 2.0, H / 2.0)));
    }

    #[test]
    fn connected_nodes_end_closer_than_unconnected() {
        // a-b tightly linked (3 parallel-ish via chain), c isolated far side.
        let g = graph(&["a", "b", "c"], &[("a", "b")]);
        let l = layout(&g);
        let d = |p: &str, q: &str| {
            let (px, py) = l.positions[p];
            let (qx, qy) = l.positions[q];
            ((px - qx).powi(2) + (py - qy).powi(2)).sqrt()
        };
        assert!(d("a", "b") < d("a", "c"), "linked pair closer than the isolated node");
    }

    #[test]
    fn helpers_are_stable() {
        assert_eq!(cluster_hue(Some(0)), cluster_hue(Some(12)), "palette cycles");
        assert_eq!(cluster_hue(None), "#9AA0A6");
        assert!(node_radius(0) < node_radius(9));
        assert!(node_radius(10_000) <= 28.0, "radius is bounded");
    }
}
