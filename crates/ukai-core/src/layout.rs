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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Children side by side, left to right.
    Horizontal,
    /// Children stacked, top to bottom.
    Vertical,
}

/// Where a moved pane goes beside the one it is dropped on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    fn direction(self) -> Direction {
        match self {
            Side::Left | Side::Right => Direction::Horizontal,
            Side::Top | Side::Bottom => Direction::Vertical,
        }
    }

    /// Whether the moved pane goes after the target in display order.
    fn after(self) -> bool {
        matches!(self, Side::Right | Side::Bottom)
    }
}

/// The smallest share of the two panes beside a dragged boundary that a
/// resize leaves either of them.
pub const MIN_SHARE: f32 = 0.05;

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
        /// Relative to the project root, or to the checkout of the workspace
        /// in [`ReportSource::Agent`].
        path: PathBuf,
        /// Where the file is read from; the project's own tree unless said
        /// otherwise, which is also what files written before panes had a
        /// source mean.
        #[serde(default, skip_serializing_if = "ReportSource::is_project")]
        source: ReportSource,
    },
    /// A web page served on `port`: a server on this machine, or one a
    /// gateway agent runs in its workspace, reached through a tunnel.
    Browser {
        project: ProjectId,
        port: u16,
        /// Where on the server the pane starts: `/`-prefixed, with any query.
        path: String,
        /// Where the port is: this machine unless said otherwise, as for a
        /// report.
        #[serde(default, skip_serializing_if = "ReportSource::is_project")]
        source: ReportSource,
    },
    /// A schedule the gateway behind `[agents.<agent>]` of `project` runs;
    /// `None` while a new one is being written and not yet saved.
    Schedule {
        project: ProjectId,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schedule: Option<String>,
    },
    /// A note of the knowledge the gateway behind `[agents.<agent>]` of
    /// `project` keeps for its repository; `None` while a new one is being
    /// written and not yet saved.
    Note {
        project: ProjectId,
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
}

/// Where a report pane reads its file from, and where a browser pane's port
/// is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReportSource {
    /// The project root on this machine.
    #[default]
    Project,
    /// The workspace of a gateway agent, served from its pod through the
    /// gateway. The agent and workspace are named rather than the pane that
    /// opened them, so the report survives a restart — and closing the agent
    /// pane — as long as the workspace is still running.
    Agent {
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<PaneWorkspace>,
    },
}

impl ReportSource {
    pub fn is_project(&self) -> bool {
        matches!(self, Self::Project)
    }

    /// The gateway agent whose workspace this reads, if any.
    pub fn agent(&self) -> Option<&str> {
        match self {
            Self::Project => None,
            Self::Agent { agent, .. } => Some(agent),
        }
    }

    pub fn workspace(&self) -> Option<&PaneWorkspace> {
        match self {
            Self::Project => None,
            Self::Agent { workspace, .. } => workspace.as_ref(),
        }
    }
}

impl PaneContent {
    pub fn project(&self) -> Option<&ProjectId> {
        match self {
            PaneContent::Empty => None,
            PaneContent::Agent { project, .. }
            | PaneContent::Report { project, .. }
            | PaneContent::Browser { project, .. }
            | PaneContent::Schedule { project, .. }
            | PaneContent::Note { project, .. } => Some(project),
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

    /// The pane that `wanted` picks, by its id and content, that is nearest
    /// the user: the
    /// focused one, else one in the active tab, else one in another, in
    /// display order. For showing something where the like of it already is,
    /// instead of opening a pane each time.
    pub fn nearest(&self, wanted: impl Fn(PaneId, &PaneContent) -> bool) -> Option<PaneId> {
        let matches = |pane: &PaneId| {
            self.content(*pane)
                .is_some_and(|content| wanted(*pane, content))
        };
        let active = self.active_tab();
        std::iter::once(self.focused)
            .chain(active.root.panes())
            .chain(
                self.tabs
                    .iter()
                    .filter(|tab| !std::ptr::eq(*tab, active))
                    .flat_map(|tab| tab.root.panes()),
            )
            .find(matches)
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

    /// Activate the tab `step` places after the active one (before it when
    /// negative), wrapping around.
    pub fn step_tab(&mut self, step: isize) {
        let index = wrap(self.active, step, self.tabs.len());
        self.select_tab(index);
    }

    /// Focus the pane `step` places after the focused one in the active tab's
    /// display order (before it when negative), wrapping around.
    pub fn step_focus(&mut self, step: isize) {
        let panes = self.active_tab().root.panes();
        if let Some(at) = panes.iter().position(|&pane| pane == self.focused) {
            self.focused = panes[wrap(at, step, panes.len())];
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
        root.insert_beside(pane, new, direction, true);
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

    /// Move the boundary between children `index` and `index + 1` of the
    /// split at `path` (child indices from the active tab's root), giving the
    /// first of them `share` of the space the two have together, clamped to
    /// [`MIN_SHARE`]. Returns whether there was such a boundary.
    pub fn resize(&mut self, path: &[usize], index: usize, share: f32) -> bool {
        if !share.is_finite() {
            return false;
        }
        let Some(Node::Split { sizes, .. }) = self.tabs[self.active].root.at_mut(path) else {
            return false;
        };
        if index + 1 >= sizes.len() {
            return false;
        }
        let share = share.clamp(MIN_SHARE, 1.0 - MIN_SHARE);
        let total = sizes[index] + sizes[index + 1];
        sizes[index] = total * share;
        sizes[index + 1] = total * (1.0 - share);
        true
    }

    /// Move `pane` to `side` of `target`, which may be in another tab, and
    /// focus it there. A tab the pane leaves empty is closed. Returns whether
    /// both panes exist and differ.
    pub fn move_pane(&mut self, pane: PaneId, target: PaneId, side: Side) -> bool {
        if pane == target || self.tab_of(target).is_none() || self.detach(pane).is_none() {
            return false;
        }
        let Some(tab) = self.tab_of(target) else {
            return false;
        };
        let root = &mut self.tabs[tab].root;
        root.insert_beside(target, pane, side.direction(), side.after());
        normalize_in_place(root);
        self.focus(pane);
        true
    }

    /// Exchange the places of two panes, which may be in different tabs, and
    /// focus `pane` in its new place. Returns whether both exist and differ.
    pub fn swap_panes(&mut self, pane: PaneId, other: PaneId) -> bool {
        if pane == other || self.tab_of(pane).is_none() || self.tab_of(other).is_none() {
            return false;
        }
        for tab in &mut self.tabs {
            tab.root.swap(pane, other);
        }
        self.focus(pane);
        true
    }

    /// Move `pane` to the right edge of tab `index`, or into a tab of its own
    /// after the others when `index` is `None`, and focus it. Returns whether
    /// the pane and tab exist.
    pub fn move_to_tab(&mut self, pane: PaneId, index: Option<usize>) -> bool {
        let Some(from) = self.tab_of(pane) else {
            return false;
        };
        let Some(index) = index else {
            self.detach(pane);
            self.tabs.push(Tab {
                root: Node::Pane(pane),
            });
            self.focus(pane);
            return true;
        };
        if index >= self.tabs.len() {
            return false;
        }
        if from == index {
            self.focus(pane);
            return true;
        }
        // Detaching closes a tab the pane was alone in, shifting later ones.
        let index = match self.detach(pane) {
            Some(Some(closed)) if closed < index => index - 1,
            _ => index,
        };
        let root = &mut self.tabs[index].root;
        let taken = std::mem::replace(root, Node::Pane(pane));
        *root = Node::Split {
            direction: Direction::Horizontal,
            children: vec![taken, Node::Pane(pane)],
            sizes: vec![1.0, 1.0],
        };
        normalize_in_place(root);
        self.focus(pane);
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

    /// Take `pane` out of its tree, keeping its content, and close the tab if
    /// that leaves it empty. `None` if the pane does not exist, else the index
    /// the closed tab had, if any. Focus is left for the caller to place.
    fn detach(&mut self, pane: PaneId) -> Option<Option<usize>> {
        let tab = self.tab_of(pane)?;
        let root = &mut self.tabs[tab].root;
        if *root == Node::Pane(pane) {
            self.tabs.remove(tab);
            if self.active > tab || self.active == self.tabs.len() {
                self.active = self.active.saturating_sub(1);
            }
            return Some(Some(tab));
        }
        root.remove(pane);
        normalize_in_place(root);
        Some(None)
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

/// Where children `index` and `index + 1` of a split with `sizes` lie, for
/// dragging the boundary between them: the offset of the first from the
/// split's start and the length of both together, in a split `extent` long
/// with a divider `gap` long between each two children. Then
/// `(pointer - offset) / length` is the share [`Layout::resize`] takes.
pub fn boundary_span(sizes: &[f32], index: usize, extent: f64, gap: f64) -> Option<(f64, f64)> {
    if index + 1 >= sizes.len() {
        return None;
    }
    let total: f64 = sizes.iter().map(|s| f64::from(*s)).sum();
    let space = (extent - gap * (sizes.len() - 1) as f64).max(0.0);
    let length = |sizes: &[f32]| sizes.iter().map(|s| f64::from(*s)).sum::<f64>() / total * space;
    let offset = length(&sizes[..index]) + gap * index as f64;
    Some((offset, length(&sizes[index..index + 2]) + gap))
}

/// `index + step` modulo `len` (which is at least 1).
fn wrap(index: usize, step: isize, len: usize) -> usize {
    let len = len.max(1) as isize;
    (index as isize + step).rem_euclid(len) as usize
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

    /// The node at `path`, child indices from this one.
    fn at_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        match path.split_first() {
            None => Some(self),
            Some((first, rest)) => match self {
                Node::Pane(_) => None,
                Node::Split { children, .. } => children.get_mut(*first)?.at_mut(rest),
            },
        }
    }

    /// Put `new` beside `target` in `direction`, after it or before it. The
    /// two share the space `target` had.
    fn insert_beside(
        &mut self,
        target: PaneId,
        new: PaneId,
        direction: Direction,
        after: bool,
    ) -> bool {
        match self {
            Node::Pane(id) if *id == target => {
                let pair = if after { [target, new] } else { [new, target] };
                *self = Node::Split {
                    direction,
                    children: pair.map(Node::Pane).into(),
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
                // Inserting along the parent's own direction adds a sibling
                // instead of nesting a one-way split inside it.
                if *own == direction
                    && let Some(index) = children
                        .iter()
                        .position(|c| matches!(c, Node::Pane(id) if *id == target))
                {
                    let half = sizes[index] / 2.0;
                    sizes[index] = half;
                    let at = if after { index + 1 } else { index };
                    sizes.insert(at, half);
                    children.insert(at, Node::Pane(new));
                    return true;
                }
                children
                    .iter_mut()
                    .any(|c| c.insert_beside(target, new, direction, after))
            }
        }
    }

    /// Exchange the leaves `a` and `b`, wherever they are in this tree.
    fn swap(&mut self, a: PaneId, b: PaneId) {
        match self {
            Node::Pane(id) if *id == a => *id = b,
            Node::Pane(id) if *id == b => *id = a,
            Node::Pane(_) => {}
            Node::Split { children, .. } => children.iter_mut().for_each(|c| c.swap(a, b)),
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
