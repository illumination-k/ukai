//! Where the nodes and edges of a flow's graph go in a pane.
//!
//! The graph reads top to bottom. Each group is laid out by itself: its
//! members go in rows, a member below the ones that lead to it, and the
//! group is as large as its rows. An edge belongs to the innermost group
//! that holds both of its ends, where it orders the two members it runs
//! between. The body of a loop sits beside its "done?" rather than below it,
//! so that the way out of the loop does not cross the body.

use std::collections::HashMap;

use super::{Graph, NodeKind};

/// The gaps between the members of a group, and between its rows.
const GAP_X: i32 = 24;
const GAP_Y: i32 = 36;
/// The space inside a group's border, and for its label above its members.
const PAD: i32 = 14;
const HEAD: i32 = 28;
/// Groups nested deeper than this are drawn as steps: a graph is someone
/// else's file, and the layout recurses once per level.
const MAX_DEPTH: usize = 64;

/// A graph as placed: everything in pixels from the top left corner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drawing {
    pub width: i32,
    pub height: i32,
    /// Every node, a group before its members.
    pub shapes: Vec<Shape>,
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub id: String,
    pub kind: NodeKind,
    /// Whether it is drawn as a group, around its members.
    pub group: bool,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// An edge as a curve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub from: String,
    pub to: String,
    /// The `d` of an SVG path.
    pub path: String,
    /// When the edge is taken, with where to write it.
    pub label: Option<String>,
    pub label_x: i32,
    pub label_y: i32,
    /// Leads back to an earlier node: the round of a loop.
    pub back: bool,
}

fn size(kind: NodeKind) -> (i32, i32) {
    match kind {
        NodeKind::Input | NodeKind::Output => (96, 28),
        NodeKind::Choice => (150, 34),
        NodeKind::Agent => (220, 76),
        _ => (220, 58),
    }
}

/// A group's members as rows, with the size they take together.
struct Rows {
    rows: Vec<Vec<usize>>,
    width: i32,
}

struct Layout<'a> {
    graph: &'a Graph,
    /// The members of each group, and of the graph itself (`None`), in the
    /// graph's order.
    members: HashMap<Option<usize>, Vec<usize>>,
    /// The edges between the members of each group, as (from, to).
    between: HashMap<Option<usize>, Vec<(usize, usize)>>,
    sizes: Vec<(i32, i32)>,
    rows: HashMap<Option<usize>, Rows>,
    shapes: Vec<Option<Shape>>,
}

impl Layout<'_> {
    fn is_group(&self, node: usize, depth: usize) -> bool {
        depth < MAX_DEPTH
            && self.graph.nodes[node].kind.is_group()
            && self.members.contains_key(&Some(node))
    }

    /// Sizes `group`'s members, then the group: the rows it takes.
    fn measure(&mut self, group: Option<usize>, depth: usize) -> (i32, i32) {
        let members = self.members.get(&group).cloned().unwrap_or_default();
        for &member in &members {
            self.sizes[member] = if self.is_group(member, depth + 1) {
                let (width, height) = self.measure(Some(member), depth + 1);
                (width + 2 * PAD, height + HEAD + PAD)
            } else {
                size(self.graph.nodes[member].kind)
            };
        }
        let is_loop = group.is_some_and(|group| self.graph.nodes[group].kind == NodeKind::Until);
        let mut row_of: HashMap<usize, usize> = HashMap::new();
        let edges = self.between.get(&group).map_or(&[][..], Vec::as_slice);
        for &member in &members {
            // The graph lists a node after those that lead to it, but for
            // the round of a loop: an edge from a later node orders nothing.
            let row = edges
                .iter()
                .filter(|(from, to)| *to == member && *from < member)
                .filter_map(|(from, _)| row_of.get(from))
                .map(|row| row + 1)
                .max()
                .unwrap_or(0);
            row_of.insert(member, if is_loop { 0 } else { row });
        }
        let count = row_of.values().max().map_or(0, |last| last + 1);
        let mut rows = vec![Vec::new(); count];
        for &member in &members {
            rows[row_of[&member]].push(member);
        }
        let width = rows
            .iter()
            .map(|row| self.row_width(row))
            .max()
            .unwrap_or(0);
        let height = rows.iter().map(|row| self.row_height(row)).sum::<i32>()
            + GAP_Y * i32::try_from(count.saturating_sub(1)).unwrap_or(0);
        self.rows.insert(group, Rows { rows, width });
        (width, height)
    }

    fn row_width(&self, row: &[usize]) -> i32 {
        let gaps = GAP_X * i32::try_from(row.len().saturating_sub(1)).unwrap_or(0);
        row.iter().map(|&member| self.sizes[member].0).sum::<i32>() + gaps
    }

    fn row_height(&self, row: &[usize]) -> i32 {
        row.iter()
            .map(|&member| self.sizes[member].1)
            .max()
            .unwrap_or(0)
    }

    /// Puts `group`'s rows with their top left corner at (`x`, `y`).
    fn place(&mut self, group: Option<usize>, x: i32, y: i32, depth: usize) {
        let Some(Rows { rows, width, .. }) = self.rows.remove(&group) else {
            return;
        };
        let mut top = y;
        for row in &rows {
            let mut left = x + (width - self.row_width(row)) / 2;
            for &member in row {
                let (member_width, member_height) = self.sizes[member];
                let is_group = self.is_group(member, depth + 1);
                let node = &self.graph.nodes[member];
                self.shapes[member] = Some(Shape {
                    id: node.id.clone(),
                    kind: node.kind,
                    group: is_group,
                    x: left,
                    y: top,
                    width: member_width,
                    height: member_height,
                });
                if is_group {
                    self.place(Some(member), left + PAD, top + HEAD, depth + 1);
                }
                left += member_width + GAP_X;
            }
            top += self.row_height(row) + GAP_Y;
        }
    }
}

/// The curve of an edge from `a` to `b`, with where its label goes.
fn curve(a: &Shape, b: &Shape, back: bool) -> (String, i32, i32) {
    let (a_mid_x, a_mid_y) = (a.x + a.width / 2, a.y + a.height / 2);
    let (b_mid_x, b_mid_y) = (b.x + b.width / 2, b.y + b.height / 2);
    if back {
        // Over the top of both, from the later node back to the earlier.
        let (start_y, end_y) = (a.y, b.y);
        let over = start_y.min(end_y) - 26;
        return (
            format!(
                "M {a_mid_x} {start_y} C {a_mid_x} {over}, {b_mid_x} {over}, {b_mid_x} {end_y}"
            ),
            (a_mid_x + b_mid_x) / 2,
            over + 8,
        );
    }
    let (start_y, end_y) = (a.y + a.height, b.y);
    if end_y >= start_y + 6 {
        let bend = (end_y - start_y) / 2;
        return (
            format!(
                "M {a_mid_x} {start_y} C {a_mid_x} {}, {b_mid_x} {}, {b_mid_x} {end_y}",
                start_y + bend,
                end_y - bend
            ),
            (a_mid_x + b_mid_x) / 2,
            (start_y + end_y) / 2,
        );
    }
    // Beside each other: out of the side that faces the other.
    let (start_x, end_x) = if b.x >= a.x {
        (a.x + a.width, b.x)
    } else {
        (a.x, b.x + b.width)
    };
    let bend = (end_x - start_x) / 2;
    (
        format!(
            "M {start_x} {a_mid_y} C {} {a_mid_y}, {} {b_mid_y}, {end_x} {b_mid_y}",
            start_x + bend,
            end_x - bend
        ),
        (start_x + end_x) / 2,
        (a_mid_y + b_mid_y) / 2 - 8,
    )
}

/// Places `graph`. A node whose group is not in the graph is drawn outside
/// any; one whose groups hold each other is left out, with its edges.
pub fn draw(graph: &Graph) -> Drawing {
    let index: HashMap<&str, usize> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(at, node)| (node.id.as_str(), at))
        .collect();
    let parent: Vec<Option<usize>> = graph
        .nodes
        .iter()
        .enumerate()
        .map(|(at, node)| {
            node.parent
                .as_deref()
                .and_then(|parent| index.get(parent).copied())
                .filter(|parent| *parent != at)
        })
        .collect();
    let mut members: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (at, parent) in parent.iter().enumerate() {
        members.entry(*parent).or_default().push(at);
    }

    // A node with the groups around it, innermost first, ending outside any.
    let chain = |node: usize| {
        let mut chain = vec![node];
        let mut at = node;
        while let Some(up) = parent[at] {
            if chain.len() > graph.nodes.len() {
                break;
            }
            chain.push(up);
            at = up;
        }
        chain
    };
    let mut between: HashMap<Option<usize>, Vec<(usize, usize)>> = HashMap::new();
    for edge in &graph.edges {
        let (Some(&from), Some(&to)) = (index.get(edge.from.as_str()), index.get(edge.to.as_str()))
        else {
            continue;
        };
        let (from_chain, to_chain) = (chain(from), chain(to));
        // From outside in, the chains are the same down to the innermost
        // group holding both ends; there the edge runs between the next two.
        let mut pairs = from_chain.iter().rev().zip(to_chain.iter().rev());
        let mut group = None;
        for (a, b) in &mut pairs {
            if a != b {
                between.entry(group).or_default().push((*a, *b));
                break;
            }
            group = Some(*a);
        }
    }

    let mut layout = Layout {
        graph,
        members,
        between,
        sizes: vec![(0, 0); graph.nodes.len()],
        rows: HashMap::new(),
        shapes: vec![None; graph.nodes.len()],
    };
    let (width, height) = layout.measure(None, 0);
    layout.place(None, PAD, PAD, 0);
    let placed = layout.shapes;

    let lines = graph
        .edges
        .iter()
        .filter_map(|edge| {
            let from = *index.get(edge.from.as_str())?;
            let to = *index.get(edge.to.as_str())?;
            let back = to <= from;
            let (path, label_x, label_y) =
                curve(placed[from].as_ref()?, placed[to].as_ref()?, back);
            Some(Line {
                from: edge.from.clone(),
                to: edge.to.clone(),
                path,
                label: edge.when.clone(),
                label_x,
                label_y,
                back,
            })
        })
        .collect();
    // A group is placed before its members, and the graph lists it before
    // them: in the graph's order, a group is drawn under what it holds.
    let shapes: Vec<Shape> = placed.into_iter().flatten().collect();
    if shapes.is_empty() {
        return Drawing::default();
    }
    Drawing {
        width: width + 2 * PAD,
        height: height + 2 * PAD,
        shapes,
        lines,
    }
}
