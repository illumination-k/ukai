//! Tabs and their pane trees, arranged by the user at runtime.
//!
//! Invariants, kept by every operation, checked by [`Layout::check`] when a
//! saved layout is loaded, and exercised by property tests:
//! - there is always at least one tab, and every tab holds at least one pane;
//! - every pane id appears exactly once across all trees and has content;
//! - a split has at least two children, one positive size per child, and no
//!   child split in the same direction (those are flattened into the parent);
//! - the focused pane is in the active tab.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use ukai_acp::PaneWorkspace;

use crate::project::ProjectId;

pub type PaneId = u64;

/// The least a pane can be shrunk to by dragging a divider, as a share of
/// the pair it is in: enough that it stays visible and grabbable.
const MIN_SHARE: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Children side by side, left to right.
    Horizontal,
    /// Children stacked, top to bottom.
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Node {
    Pane(PaneId),
    Split {
        direction: Direction,
        children: Vec<Node>,
        /// Relative size of each child; rendered as flex weights.
        sizes: Vec<f32>,
    },
}

/// What a pane shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaneContent {
    /// Freshly opened; the user picks what goes in it.
    Empty,
    /// A session with `[agents.<agent>]` of `project`.
    Agent {
        project: ProjectId,
        agent: String,
        /// ACP session id once the agent is ready, used to continue the
        /// session after a restart.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        /// For a gateway agent: the workspace the pane's connections ask for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<PaneWorkspace>,
    },
    Report {
        project: ProjectId,
        path: PathBuf,
    },
}

impl PaneContent {
    pub fn project(&self) -> Option<&ProjectId> {
        match self {
            PaneContent::Empty => None,
            PaneContent::Agent { project, .. } | PaneContent::Report { project, .. } => {
                Some(project)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tab {
    pub root: Node,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    tabs: Vec<Tab>,
    active: usize,
    focused: PaneId,
    contents: BTreeMap<PaneId, PaneContent>,
    next_id: PaneId,
}

impl Default for Layout {
    fn default() -> Self {
        let mut layout = Self {
            tabs: Vec::new(),
            active: 0,
            focused: 0,
            contents: BTreeMap::new(),
            next_id: 0,
        };
        layout.new_tab();
        layout
    }
}

impl Layout {
    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn active(&self) -> usize {
        self.active
    }

    pub fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    pub fn focused(&self) -> PaneId {
        self.focused
    }

    pub fn content(&self, pane: PaneId) -> Option<&PaneContent> {
        self.contents.get(&pane)
    }

    /// Every pane with its content, in id order.
    pub fn contents(&self) -> impl Iterator<Item = (PaneId, &PaneContent)> {
        self.contents.iter().map(|(id, content)| (*id, content))
    }

    /// Open a tab holding one empty pane, make it active, and focus the pane.
    pub fn new_tab(&mut self) -> PaneId {
        let pane = self.alloc();
        self.tabs.push(Tab {
            root: Node::Pane(pane),
        });
        self.active = self.tabs.len() - 1;
        self.focused = pane;
        pane
    }

    pub fn select_tab(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get(index) {
            self.active = index;
            if !tab.root.contains(self.focused) {
                self.focused = tab.root.first_pane();
            }
        }
    }

    /// Close a tab; returns the panes that went with it.
    pub fn close_tab(&mut self, index: usize) -> Vec<PaneId> {
        if index >= self.tabs.len() {
            return Vec::new();
        }
        let removed = self.tabs.remove(index).root.panes();
        for pane in &removed {
            self.contents.remove(pane);
        }
        if self.tabs.is_empty() {
            self.new_tab();
        } else {
            if self.active > index || self.active == self.tabs.len() {
                self.active -= 1;
            }
            self.focused = self.active_tab().root.first_pane();
        }
        removed
    }

    /// Split `pane`, opening an empty pane after it in `direction`, and focus
    /// the new pane. The two halves share the space `pane` had. `None` if
    /// `pane` does not exist.
    pub fn split(&mut self, pane: PaneId, direction: Direction) -> Option<PaneId> {
        let tab = self.tab_of(pane)?;
        let new = self.alloc();
        let root = &mut self.tabs[tab].root;
        root.split(pane, direction, new);
        normalize_in_place(root);
        self.active = tab;
        self.focused = new;
        Some(new)
    }

    /// Close `pane`. Focus moves to the pane before it (or the next one); a tab
    /// left empty is closed. Returns whether the pane existed.
    pub fn close_pane(&mut self, pane: PaneId) -> bool {
        let Some(tab) = self.tab_of(pane) else {
            return false;
        };
        let order = self.tabs[tab].root.panes();
        let Some(position) = order.iter().position(|id| *id == pane) else {
            return false;
        };
        if order.len() == 1 {
            self.close_tab(tab);
            return true;
        }
        let root = &mut self.tabs[tab].root;
        root.remove(pane);
        normalize_in_place(root);
        self.contents.remove(&pane);
        if self.focused == pane {
            self.focused = order[if position == 0 { 1 } else { position - 1 }];
        }
        true
    }

    pub fn set_content(&mut self, pane: PaneId, content: PaneContent) {
        if let Some(slot) = self.contents.get_mut(&pane) {
            *slot = content;
        }
    }

    /// Remember the ACP session an agent pane is running.
    pub fn record_session(&mut self, pane: PaneId, session_id: String) {
        if let Some(PaneContent::Agent { session, .. }) = self.contents.get_mut(&pane) {
            *session = Some(session_id);
        }
    }

    /// Focus `pane`, switching to its tab.
    pub fn focus(&mut self, pane: PaneId) {
        if let Some(tab) = self.tab_of(pane) {
            self.active = tab;
            self.focused = pane;
        }
    }

    /// The agent panes of the active tab, in display order: what a prompt
    /// sent to the whole tab goes to.
    pub fn active_agent_panes(&self) -> Vec<PaneId> {
        self.active_tab()
            .root
            .panes()
            .into_iter()
            .filter(|pane| matches!(self.content(*pane), Some(PaneContent::Agent { .. })))
            .collect()
    }

    /// Where focus goes from `pane` when moving `direction`, `forward` being
    /// right or down: the neighbouring pane across the innermost divider that
    /// runs that way. `None` when there is nothing that way.
    pub fn neighbour(&self, pane: PaneId, direction: Direction, forward: bool) -> Option<PaneId> {
        let tab = self.tab_of(pane)?;
        let mut path = Vec::new();
        self.tabs[tab].root.path_to(pane, &mut path).then_some(())?;
        // From the parent split outwards, take the first one that runs the
        // right way and has a sibling on that side.
        for depth in (0..path.len()).rev() {
            let Some(Node::Split {
                direction: own,
                children,
                ..
            }) = self.tabs[tab].root.at(&path[..depth])
            else {
                continue;
            };
            if *own != direction {
                continue;
            }
            let index = path[depth];
            let sibling = if forward {
                index.checked_add(1).filter(|next| *next < children.len())
            } else {
                index.checked_sub(1)
            };
            if let Some(sibling) = sibling.and_then(|index| children.get(index)) {
                return Some(if forward {
                    sibling.first_pane()
                } else {
                    sibling.last_pane()
                });
            }
        }
        None
    }

    /// Move the divider between children `index` and `index + 1` of the split
    /// at `path` in the active tab: `first` is the share of their combined
    /// size that goes to the earlier child, clamped so neither collapses.
    /// Returns whether the split was there.
    pub fn resize(&mut self, path: &[usize], index: usize, first: f32) -> bool {
        let active = self.active;
        let Some(Node::Split { sizes, .. }) = self.tabs[active].root.at_mut(path) else {
            return false;
        };
        let Some(pair) = sizes
            .get(index)
            .zip(sizes.get(index + 1))
            .map(|(a, b)| a + b)
        else {
            return false;
        };
        if !first.is_finite() {
            return false;
        }
        let share = first.clamp(MIN_SHARE, 1.0 - MIN_SHARE);
        sizes[index] = pair * share;
        sizes[index + 1] = pair * (1.0 - share);
        true
    }

    /// Verify the invariants listed in the module docs, e.g. on a layout read
    /// back from disk.
    pub fn check(&self) -> Result<(), String> {
        if self.tabs.is_empty() {
            return Err("no tabs".into());
        }
        if self.active >= self.tabs.len() {
            return Err(format!("active tab {} out of range", self.active));
        }
        let mut seen = BTreeSet::new();
        for tab in &self.tabs {
            tab.root.check(None)?;
            for pane in tab.root.panes() {
                if !seen.insert(pane) {
                    return Err(format!("pane {pane} appears twice"));
                }
                if pane >= self.next_id {
                    return Err(format!("pane {pane} not below next id {}", self.next_id));
                }
            }
        }
        if !seen.iter().eq(self.contents.keys()) {
            return Err("pane contents do not match the trees".into());
        }
        if !self.active_tab().root.contains(self.focused) {
            return Err(format!(
                "focused pane {} not in the active tab",
                self.focused
            ));
        }
        Ok(())
    }

    fn tab_of(&self, pane: PaneId) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.root.contains(pane))
    }

    fn alloc(&mut self) -> PaneId {
        let id = self.next_id;
        self.next_id += 1;
        self.contents.insert(id, PaneContent::Empty);
        id
    }
}

impl Node {
    /// Pane ids in display order (depth first, left to right / top to bottom).
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Pane(id) => out.push(*id),
            Node::Split { children, .. } => children.iter().for_each(|c| c.collect(out)),
        }
    }

    fn first_pane(&self) -> PaneId {
        match self {
            Node::Pane(id) => *id,
            // Normalized splits are never empty; fall back to an id no pane has.
            Node::Split { children, .. } => children.first().map_or(PaneId::MAX, Node::first_pane),
        }
    }

    fn last_pane(&self) -> PaneId {
        match self {
            Node::Pane(id) => *id,
            Node::Split { children, .. } => children.last().map_or(PaneId::MAX, Node::last_pane),
        }
    }

    /// The child indices leading from here to `pane`, pushed onto `path`.
    fn path_to(&self, pane: PaneId, path: &mut Vec<usize>) -> bool {
        match self {
            Node::Pane(id) => *id == pane,
            Node::Split { children, .. } => children.iter().enumerate().any(|(index, child)| {
                path.push(index);
                child.path_to(pane, path) || {
                    path.pop();
                    false
                }
            }),
        }
    }

    /// The node `path` leads to, the root itself for an empty path.
    fn at(&self, path: &[usize]) -> Option<&Node> {
        match path.split_first() {
            None => Some(self),
            Some((index, rest)) => match self {
                Node::Pane(_) => None,
                Node::Split { children, .. } => children.get(*index)?.at(rest),
            },
        }
    }

    fn at_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        match path.split_first() {
            None => Some(self),
            Some((index, rest)) => match self {
                Node::Pane(_) => None,
                Node::Split { children, .. } => children.get_mut(*index)?.at_mut(rest),
            },
        }
    }

    fn contains(&self, pane: PaneId) -> bool {
        match self {
            Node::Pane(id) => *id == pane,
            Node::Split { children, .. } => children.iter().any(|c| c.contains(pane)),
        }
    }

    fn check(&self, parent: Option<Direction>) -> Result<(), String> {
        let Node::Split {
            direction,
            children,
            sizes,
        } = self
        else {
            return Ok(());
        };
        if children.len() < 2 {
            return Err(format!("split with {} children", children.len()));
        }
        if sizes.len() != children.len() || !sizes.iter().all(|s| s.is_finite() && *s > 0.0) {
            return Err(format!("bad sizes {sizes:?}"));
        }
        if parent == Some(*direction) {
            return Err("split nested in a split of the same direction".into());
        }
        children.iter().try_for_each(|c| c.check(Some(*direction)))
    }

    fn split(&mut self, target: PaneId, direction: Direction, new: PaneId) -> bool {
        match self {
            Node::Pane(id) if *id == target => {
                *self = Node::Split {
                    direction,
                    children: vec![Node::Pane(target), Node::Pane(new)],
                    sizes: vec![1.0, 1.0],
                };
                true
            }
            Node::Pane(_) => false,
            Node::Split {
                direction: own,
                children,
                sizes,
            } => {
                // Splitting along the parent's own direction adds a sibling
                // instead of nesting a one-way split inside it.
                if *own == direction
                    && let Some(index) = children
                        .iter()
                        .position(|c| matches!(c, Node::Pane(id) if *id == target))
                {
                    let half = sizes[index] / 2.0;
                    sizes[index] = half;
                    sizes.insert(index + 1, half);
                    children.insert(index + 1, Node::Pane(new));
                    return true;
                }
                children.iter_mut().any(|c| c.split(target, direction, new))
            }
        }
    }

    fn remove(&mut self, target: PaneId) {
        if let Node::Split {
            children, sizes, ..
        } = self
        {
            if let Some(index) = children
                .iter()
                .position(|c| matches!(c, Node::Pane(id) if *id == target))
            {
                children.remove(index);
                sizes.remove(index);
            }
            children.iter_mut().for_each(|c| c.remove(target));
        }
    }
}

/// Drop empty splits, collapse single-child splits, and flatten children
/// that split in their parent's direction (scaling their sizes into the
/// slot they occupied).
fn normalize(node: Node) -> Option<Node> {
    let Node::Split {
        direction,
        children,
        sizes,
    } = node
    else {
        return Some(node);
    };
    let mut flat_children = Vec::with_capacity(children.len());
    let mut flat_sizes = Vec::with_capacity(children.len());
    for (child, size) in children.into_iter().zip(sizes) {
        match normalize(child) {
            None => {}
            Some(Node::Split {
                direction: inner,
                children: grand,
                sizes: grand_sizes,
            }) if inner == direction => {
                let total: f32 = grand_sizes.iter().sum();
                flat_children.extend(grand);
                flat_sizes.extend(grand_sizes.iter().map(|g| size * g / total));
            }
            Some(other) => {
                flat_children.push(other);
                flat_sizes.push(size);
            }
        }
    }
    match flat_children.len() {
        0 => None,
        1 => flat_children.pop(),
        _ => Some(Node::Split {
            direction,
            children: flat_children,
            sizes: flat_sizes,
        }),
    }
}

fn normalize_in_place(root: &mut Node) {
    let taken = std::mem::replace(root, Node::Pane(PaneId::MAX));
    // Callers only normalize trees that still hold a pane.
    if let Some(normalized) = normalize(taken) {
        *root = normalized;
    }
}

#[cfg(test)]
mod tests;
