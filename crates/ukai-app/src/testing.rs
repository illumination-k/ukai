//! Components run without a window, for tests: a [`Screen`] renders a
//! `VirtualDom` into an in-memory [`Dom`], sends it the events a user's
//! mouse and keyboard would, and lets a test look at what is shown.
//!
//! Nothing lays the page out, so what needs an element's place on screen
//! (`onmounted`, dragging a split's boundary) is out of reach here.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::rc::Rc;
use std::str::FromStr;
use std::sync::Once;

use dioxus::core::{AttributeValue, ElementId, Template, TemplateAttribute, TemplateNode};
use dioxus::dioxus_core::WriteMutations;
use dioxus::html::geometry::{ClientPoint, Coordinates, ElementPoint, PagePoint, ScreenPoint};
use dioxus::html::input_data::{MouseButton, MouseButtonSet};
use dioxus::html::{
    PlatformEventData, SerializedFormData, SerializedHtmlEventConverter, SerializedKeyboardData,
    SerializedMouseData, set_event_converter,
};
use dioxus::prelude::*;

/// A node of a [`Dom`]. It stays valid until the node is removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeRef(usize);

/// One thing an element holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Element(NodeRef),
    Text(String),
}

#[derive(Debug)]
enum Kind {
    Element {
        tag: &'static str,
        /// Style properties are kept as `style:<property>`.
        attributes: BTreeMap<String, String>,
        listeners: BTreeSet<&'static str>,
    },
    Text(String),
    /// Where a dynamic node that renders nothing would go.
    Placeholder,
}

#[derive(Debug)]
struct NodeData {
    parent: Option<usize>,
    children: Vec<usize>,
    kind: Kind,
    /// The id the `VirtualDom` knows the node by, if it has one.
    id: Option<ElementId>,
}

/// The document a `VirtualDom` writes its mutations to, as a web view's
/// interpreter would keep it.
#[derive(Debug)]
pub struct Dom {
    nodes: Vec<NodeData>,
    ids: HashMap<usize, usize>,
    stack: Vec<usize>,
    /// How many mutations were written so far.
    edits: usize,
}

impl Default for Dom {
    fn default() -> Self {
        let root = NodeData {
            parent: None,
            children: Vec::new(),
            kind: Kind::Element {
                tag: "#root",
                attributes: BTreeMap::new(),
                listeners: BTreeSet::new(),
            },
            id: Some(ElementId(0)),
        };
        Self {
            nodes: vec![root],
            ids: HashMap::from([(0, 0)]),
            stack: Vec::new(),
            edits: 0,
        }
    }
}

impl Dom {
    pub fn root(&self) -> NodeRef {
        NodeRef(0)
    }

    /// The element children of `node`.
    pub fn children(&self, node: NodeRef) -> Vec<NodeRef> {
        self.nodes[node.0]
            .children
            .iter()
            .filter(|child| matches!(self.nodes[**child].kind, Kind::Element { .. }))
            .map(|child| NodeRef(*child))
            .collect()
    }

    /// What `node` holds, in order: its elements and its text.
    pub fn content(&self, node: NodeRef) -> Vec<Content> {
        self.nodes[node.0]
            .children
            .iter()
            .filter_map(|child| match &self.nodes[*child].kind {
                Kind::Element { .. } => Some(Content::Element(NodeRef(*child))),
                Kind::Text(text) => Some(Content::Text(text.clone())),
                Kind::Placeholder => None,
            })
            .collect()
    }

    pub fn tag(&self, node: NodeRef) -> &'static str {
        match &self.nodes[node.0].kind {
            Kind::Element { tag, .. } => tag,
            _ => "",
        }
    }

    pub fn attribute(&self, node: NodeRef, name: &str) -> Option<&str> {
        match &self.nodes[node.0].kind {
            Kind::Element { attributes, .. } => attributes.get(name).map(String::as_str),
            _ => None,
        }
    }

    /// The names of `node`'s attributes.
    pub fn attributes(&self, node: NodeRef) -> Vec<&str> {
        match &self.nodes[node.0].kind {
            Kind::Element { attributes, .. } => attributes.keys().map(String::as_str).collect(),
            _ => Vec::new(),
        }
    }

    pub fn has_class(&self, node: NodeRef, class: &str) -> bool {
        self.attribute(node, "class")
            .is_some_and(|classes| classes.split_whitespace().any(|c| c == class))
    }

    /// The text under `node`, in document order.
    pub fn text(&self, node: NodeRef) -> String {
        let mut out = String::new();
        self.walk(node.0, &mut |dom, index| {
            if let Kind::Text(text) = &dom.nodes[index].kind {
                out.push_str(text);
            }
        });
        out
    }

    /// The elements under `node` (itself excluded) that `wanted` accepts, in
    /// document order.
    pub fn find_in(&self, node: NodeRef, wanted: impl Fn(&Dom, NodeRef) -> bool) -> Vec<NodeRef> {
        let mut found = Vec::new();
        self.walk(node.0, &mut |dom, index| {
            let candidate = NodeRef(index);
            if index != node.0
                && matches!(dom.nodes[index].kind, Kind::Element { .. })
                && wanted(dom, candidate)
            {
                found.push(candidate);
            }
        });
        found
    }

    /// The elements of class `class` under `node`, in document order.
    pub fn by_class_in(&self, node: NodeRef, class: &str) -> Vec<NodeRef> {
        self.find_in(node, |dom, candidate| dom.has_class(candidate, class))
    }

    /// The elements of class `class`, in document order.
    pub fn by_class(&self, class: &str) -> Vec<NodeRef> {
        self.by_class_in(self.root(), class)
    }

    /// Every element of the document, in document order.
    pub fn elements(&self) -> Vec<NodeRef> {
        self.find_in(self.root(), |_, _| true)
    }

    /// The document written out: elements with their attributes and the
    /// events they listen to, and text. Two documents that read the same
    /// show the same and react to the same.
    pub fn html(&self) -> String {
        self.html_without(|_, _| false)
    }

    /// [`Self::html`] without the elements `skipped` picks, and what is
    /// under them.
    pub fn html_without(&self, skipped: impl Fn(&Dom, NodeRef) -> bool) -> String {
        let mut out = String::new();
        for child in &self.nodes[0].children {
            self.write(*child, &skipped, &mut out);
        }
        out
    }

    fn write(&self, index: usize, skipped: &impl Fn(&Dom, NodeRef) -> bool, out: &mut String) {
        let node = &self.nodes[index];
        match &node.kind {
            Kind::Placeholder => {}
            Kind::Text(text) => out.push_str(text),
            Kind::Element { .. } if skipped(self, NodeRef(index)) => {}
            Kind::Element {
                tag,
                attributes,
                listeners,
            } => {
                let _ = write!(out, "<{tag}");
                for (name, value) in attributes {
                    let _ = write!(out, " {name}={value:?}");
                }
                for listener in listeners {
                    let _ = write!(out, " @{listener}");
                }
                out.push('>');
                for child in &node.children {
                    self.write(*child, skipped, out);
                }
                let _ = write!(out, "</{tag}>");
            }
        }
    }

    fn walk(&self, index: usize, visit: &mut impl FnMut(&Dom, usize)) {
        visit(self, index);
        for child in &self.nodes[index].children {
            self.walk(*child, visit);
        }
    }

    /// The nearest of `node` and its ancestors that listens to `event`.
    fn listener(&self, node: NodeRef, event: &str) -> Option<ElementId> {
        let mut at = Some(node.0);
        while let Some(index) = at {
            let data = &self.nodes[index];
            if let (Kind::Element { listeners, .. }, Some(id)) = (&data.kind, data.id)
                && listeners.contains(event)
            {
                return Some(id);
            }
            at = data.parent;
        }
        None
    }

    /// `node`'s own id, if it listens to `event` itself.
    fn own_listener(&self, node: NodeRef, event: &str) -> Option<ElementId> {
        let data = &self.nodes[node.0];
        match (&data.kind, data.id) {
            (Kind::Element { listeners, .. }, Some(id)) if listeners.contains(event) => Some(id),
            _ => None,
        }
    }

    fn add(&mut self, kind: Kind) -> usize {
        self.nodes.push(NodeData {
            parent: None,
            children: Vec::new(),
            kind,
            id: None,
        });
        self.nodes.len() - 1
    }

    fn identify(&mut self, index: usize, id: ElementId) {
        self.nodes[index].id = Some(id);
        self.ids.insert(id.0, index);
    }

    fn known(&self, id: ElementId) -> usize {
        *self
            .ids
            .get(&id.0)
            .unwrap_or_else(|| panic!("no node has {id:?}"))
    }

    fn build(&mut self, template: &TemplateNode) -> usize {
        match template {
            TemplateNode::Element {
                tag,
                attrs,
                children,
                ..
            } => {
                let attributes = attrs
                    .iter()
                    .filter_map(|attribute| match attribute {
                        TemplateAttribute::Static {
                            name,
                            value,
                            namespace,
                        } => Some((attribute_key(name, *namespace), (*value).to_owned())),
                        TemplateAttribute::Dynamic { .. } => None,
                    })
                    .collect();
                let index = self.add(Kind::Element {
                    tag,
                    attributes,
                    listeners: BTreeSet::new(),
                });
                for child in *children {
                    let child = self.build(child);
                    self.nodes[child].parent = Some(index);
                    self.nodes[index].children.push(child);
                }
                index
            }
            TemplateNode::Text { text } => self.add(Kind::Text((*text).to_owned())),
            TemplateNode::Dynamic { .. } => self.add(Kind::Placeholder),
        }
    }

    /// The node at `path`, child indices from the node on top of the stack.
    fn at_path(&self, path: &[u8]) -> usize {
        let top = *self
            .stack
            .last()
            .unwrap_or_else(|| panic!("a node on the stack"));
        path.iter().fold(top, |node, step| {
            self.nodes[node].children[usize::from(*step)]
        })
    }

    /// Take the `count` nodes on top of the stack to put them somewhere.
    /// One that is in the document already moves: it leaves where it was.
    fn pop(&mut self, count: usize) -> Vec<usize> {
        let nodes = self.stack.split_off(self.stack.len() - count);
        for node in &nodes {
            if self.nodes[*node].parent.is_some() {
                self.detach(*node);
            }
        }
        nodes
    }

    fn detach(&mut self, index: usize) -> (usize, usize) {
        let parent = self.nodes[index]
            .parent
            .take()
            .unwrap_or_else(|| panic!("an attached node"));
        let position = self.nodes[parent]
            .children
            .iter()
            .position(|child| *child == index)
            .unwrap_or_else(|| panic!("a child of its parent"));
        self.nodes[parent].children.remove(position);
        (parent, position)
    }

    fn insert(&mut self, parent: usize, position: usize, nodes: Vec<usize>) {
        for node in &nodes {
            self.nodes[*node].parent = Some(parent);
        }
        self.nodes[parent]
            .children
            .splice(position..position, nodes);
    }

    fn position(&self, index: usize) -> (usize, usize) {
        let parent = self.nodes[index]
            .parent
            .unwrap_or_else(|| panic!("an attached node"));
        let position = self.nodes[parent]
            .children
            .iter()
            .position(|child| *child == index)
            .unwrap_or_else(|| panic!("a child of its parent"));
        (parent, position)
    }
}

fn attribute_key(name: &str, namespace: Option<&str>) -> String {
    match namespace {
        Some(namespace) => format!("{namespace}:{name}"),
        None => name.to_owned(),
    }
}

impl WriteMutations for Dom {
    fn append_children(&mut self, id: ElementId, m: usize) {
        self.edits += 1;
        let nodes = self.pop(m);
        let parent = self.known(id);
        let end = self.nodes[parent].children.len();
        self.insert(parent, end, nodes);
    }

    fn assign_node_id(&mut self, path: &'static [u8], id: ElementId) {
        self.edits += 1;
        let node = self.at_path(path);
        self.identify(node, id);
    }

    fn create_placeholder(&mut self, id: ElementId) {
        self.edits += 1;
        let node = self.add(Kind::Placeholder);
        self.identify(node, id);
        self.stack.push(node);
    }

    fn create_text_node(&mut self, value: &str, id: ElementId) {
        self.edits += 1;
        let node = self.add(Kind::Text(value.to_owned()));
        self.identify(node, id);
        self.stack.push(node);
    }

    fn load_template(&mut self, template: Template, index: usize, id: ElementId) {
        self.edits += 1;
        let node = self.build(&template.roots[index]);
        self.identify(node, id);
        self.stack.push(node);
    }

    fn replace_node_with(&mut self, id: ElementId, m: usize) {
        self.edits += 1;
        let nodes = self.pop(m);
        let old = self.known(id);
        let (parent, position) = self.detach(old);
        self.insert(parent, position, nodes);
    }

    fn replace_placeholder_with_nodes(&mut self, path: &'static [u8], m: usize) {
        self.edits += 1;
        let nodes = self.pop(m);
        let old = self.at_path(path);
        let (parent, position) = self.detach(old);
        self.insert(parent, position, nodes);
    }

    fn insert_nodes_after(&mut self, id: ElementId, m: usize) {
        self.edits += 1;
        let nodes = self.pop(m);
        let (parent, position) = self.position(self.known(id));
        self.insert(parent, position + 1, nodes);
    }

    fn insert_nodes_before(&mut self, id: ElementId, m: usize) {
        self.edits += 1;
        let nodes = self.pop(m);
        let (parent, position) = self.position(self.known(id));
        self.insert(parent, position, nodes);
    }

    fn set_attribute(
        &mut self,
        name: &'static str,
        ns: Option<&'static str>,
        value: &AttributeValue,
        id: ElementId,
    ) {
        self.edits += 1;
        let node = self.known(id);
        let Kind::Element { attributes, .. } = &mut self.nodes[node].kind else {
            panic!("an attribute set on what is not an element");
        };
        let key = attribute_key(name, ns);
        let value = match value {
            AttributeValue::Text(text) => Some(text.clone()),
            AttributeValue::Float(number) => Some(number.to_string()),
            AttributeValue::Int(number) => Some(number.to_string()),
            AttributeValue::Bool(flag) => Some(flag.to_string()),
            AttributeValue::None => None,
            AttributeValue::Listener(_) | AttributeValue::Any(_) => {
                panic!("`{key}` is not something a document holds")
            }
        };
        match value {
            Some(value) => attributes.insert(key, value),
            None => attributes.remove(&key),
        };
    }

    fn set_node_text(&mut self, value: &str, id: ElementId) {
        self.edits += 1;
        let node = self.known(id);
        self.nodes[node].kind = Kind::Text(value.to_owned());
    }

    fn create_event_listener(&mut self, name: &'static str, id: ElementId) {
        self.edits += 1;
        let node = self.known(id);
        if let Kind::Element { listeners, .. } = &mut self.nodes[node].kind {
            listeners.insert(name);
        }
    }

    fn remove_event_listener(&mut self, name: &'static str, id: ElementId) {
        self.edits += 1;
        let node = self.known(id);
        if let Kind::Element { listeners, .. } = &mut self.nodes[node].kind {
            listeners.remove(name);
        }
    }

    fn remove_node(&mut self, id: ElementId) {
        self.edits += 1;
        let node = self.known(id);
        self.detach(node);
    }

    fn push_root(&mut self, id: ElementId) {
        self.edits += 1;
        let node = self.known(id);
        self.stack.push(node);
    }
}

/// A key pressed with modifiers, as a `keydown` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPress {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    /// The DOM's `key`: `"d"`, `"Tab"`.
    pub key: &'static str,
    /// The DOM's `code`: `"KeyD"`, `"Tab"`.
    pub code: &'static str,
    /// Whether an input method is taking the key for a composition.
    pub composing: bool,
}

/// How many renders in a row may each change the screen before it counts
/// as never settling.
const RENDERS: usize = 32;

/// A component tree rendered into a [`Dom`], with the user's hands on it.
pub struct Screen {
    vdom: VirtualDom,
    pub dom: Dom,
    /// What components that sleep or spawn find when they look for tokio.
    /// Nothing drives it, so no timer fires and no task runs: the screen
    /// changes only by what the test does.
    runtime: tokio::runtime::Runtime,
}

impl Screen {
    /// Render `vdom` for the first time.
    pub fn new(mut vdom: VirtualDom) -> Self {
        static CONVERTER: Once = Once::new();
        CONVERTER.call_once(|| set_event_converter(Box::new(SerializedHtmlEventConverter)));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap_or_else(|error| panic!("no tokio runtime: {error}"));
        let mut dom = Dom::default();
        {
            let _tokio = runtime.enter();
            vdom.rebuild(&mut dom);
        }
        let mut screen = Self { vdom, dom, runtime };
        screen.render();
        screen
    }

    /// Show whatever changed since the last render. An effect runs after
    /// the render it follows, and what it changes takes another, as the
    /// app's event loop would give it; so this renders until two renders in
    /// a row change nothing: the first may only have run the effects that
    /// were waiting.
    pub fn render(&mut self) {
        let _tokio = self.runtime.enter();
        let mut quiet = 0;
        for _ in 0..RENDERS {
            let before = self.dom.edits;
            self.vdom.render_immediate(&mut self.dom);
            assert!(self.dom.stack.is_empty(), "nodes left unplaced");
            quiet = if self.dom.edits == before {
                quiet + 1
            } else {
                0
            };
            if quiet == 2 {
                return;
            }
        }
        panic!("the screen keeps changing after {RENDERS} renders");
    }

    /// Run `f` where the root component's signals and contexts can be read
    /// and written.
    pub fn with<T>(&self, f: impl FnOnce() -> T) -> T {
        let _tokio = self.runtime.enter();
        self.vdom.in_scope(ScopeId::APP, f)
    }

    /// Send `event` to `node` and render what it changed: from there up as
    /// in a browser if it `bubbles`, to `node` alone if not. Returns whether
    /// anything was listening.
    fn send(&mut self, node: NodeRef, event: &str, bubbles: bool, data: PlatformEventData) -> bool {
        let id = if bubbles {
            self.dom.listener(node, event)
        } else {
            self.dom.own_listener(node, event)
        };
        let Some(id) = id else {
            return false;
        };
        let data: Rc<dyn Any> = Rc::new(data);
        let tokio = self.runtime.enter();
        self.vdom
            .runtime()
            .handle_event(event, Event::new(data, bubbles), id);
        drop(tokio);
        self.render();
        true
    }

    fn mouse(
        &mut self,
        node: NodeRef,
        event: &str,
        button: Option<MouseButton>,
        held: Option<MouseButton>,
        at: (f64, f64),
    ) -> bool {
        let mut buttons = MouseButtonSet::empty();
        if let Some(held) = held {
            buttons |= held;
        }
        let coordinates = Coordinates::new(
            ScreenPoint::new(at.0, at.1),
            ClientPoint::new(at.0, at.1),
            ElementPoint::new(0.0, 0.0),
            PagePoint::new(at.0, at.1),
        );
        let data = SerializedMouseData::new(button, buttons, coordinates, Modifiers::empty());
        let bubbles = !matches!(event, "mouseenter" | "mouseleave");
        self.send(node, event, bubbles, PlatformEventData::new(Box::new(data)))
    }

    /// Press `button` on `node` with the pointer at `at`.
    pub fn mouse_down(&mut self, node: NodeRef, button: MouseButton, at: (f64, f64)) -> bool {
        self.mouse(node, "mousedown", Some(button), Some(button), at)
    }

    /// Move the pointer over `node` to `at`, with `held` down if any.
    pub fn mouse_move(&mut self, node: NodeRef, held: Option<MouseButton>, at: (f64, f64)) -> bool {
        self.mouse(node, "mousemove", None, held, at)
    }

    /// Release `button` over `node`.
    pub fn mouse_up(&mut self, node: NodeRef, button: MouseButton, at: (f64, f64)) -> bool {
        self.mouse(node, "mouseup", Some(button), None, at)
    }

    /// The pointer comes over `node`, with `held` down if any.
    pub fn mouse_enter(&mut self, node: NodeRef, held: Option<MouseButton>) -> bool {
        self.mouse(node, "mouseenter", None, held, (0.0, 0.0))
    }

    /// The pointer leaves `node`, with `held` down if any.
    pub fn mouse_leave(&mut self, node: NodeRef, held: Option<MouseButton>) -> bool {
        self.mouse(node, "mouseleave", None, held, (0.0, 0.0))
    }

    /// The `click` a press and release of the primary button on the same
    /// element end with.
    pub fn click(&mut self, node: NodeRef) -> bool {
        self.mouse(node, "click", Some(MouseButton::Primary), None, (0.0, 0.0))
    }

    /// The text field `node` now holds `value`, as after typing or deleting.
    pub fn input(&mut self, node: NodeRef, value: &str) -> bool {
        let data = SerializedFormData::new(value.to_owned(), Vec::new());
        self.send(node, "input", true, PlatformEventData::new(Box::new(data)))
    }

    pub fn double_click(&mut self, node: NodeRef) -> bool {
        self.mouse(
            node,
            "dblclick",
            Some(MouseButton::Primary),
            None,
            (0.0, 0.0),
        )
    }

    pub fn key_down(&mut self, node: NodeRef, press: &KeyPress) -> bool {
        let mut modifiers = Modifiers::empty();
        for (on, modifier) in [
            (press.ctrl, Modifiers::CONTROL),
            (press.alt, Modifiers::ALT),
            (press.shift, Modifiers::SHIFT),
            (press.meta, Modifiers::META),
        ] {
            if on {
                modifiers |= modifier;
            }
        }
        let key = if press.key.chars().count() == 1 {
            Key::Character(press.key.to_owned())
        } else {
            Key::from_str(press.key)
                .unwrap_or_else(|_| panic!("`{}` is not a DOM key value", press.key))
        };
        let code = Code::from_str(press.code)
            .unwrap_or_else(|_| panic!("`{}` is not a DOM key code", press.code));
        let data = SerializedKeyboardData::new(
            key,
            code,
            Location::Standard,
            false,
            modifiers,
            press.composing,
        );
        self.send(
            node,
            "keydown",
            true,
            PlatformEventData::new(Box::new(data)),
        )
    }
}
