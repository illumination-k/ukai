//! An agent's message on screen, whatever the agent wrote: nothing of it
//! turns into live HTML, none of its text is lost, and a message that
//! arrived in pieces reads as one that arrived whole.

use proptest::prelude::*;

use super::*;
use crate::testing::{Dom, NodeRef, Screen};

/// A message whose source the test sets through the `Signal` it provides.
#[component]
fn Message() -> Element {
    let source = use_context_provider(|| Signal::new(String::new()));
    rsx! {
        Markdown { source: source() }
    }
}

struct Shown {
    screen: Screen,
    source: Signal<String>,
}

impl Shown {
    fn new() -> Self {
        let screen = Screen::new(VirtualDom::new(Message));
        let source = screen.with(consume_context::<Signal<String>>);
        Self { screen, source }
    }

    fn of(text: &str) -> Self {
        let mut shown = Self::new();
        shown.set(text);
        shown
    }

    fn set(&mut self, text: &str) {
        let mut source = self.source;
        self.screen.with(|| source.set(text.to_owned()));
        self.screen.render();
    }
}

/// Pieces of Markdown, of HTML an agent might pass along or be tricked into
/// writing, and of anything else, to be joined in any order.
fn piece() -> impl Strategy<Value = String> {
    const SYNTAX: &[&str] = &[
        "\n",
        "\n\n",
        " ",
        "# ",
        "### ",
        "- ",
        "  - ",
        "1. ",
        "- [ ] ",
        "- [x] ",
        "> ",
        "---\n",
        "```",
        "```rust\n",
        "`",
        "*",
        "**",
        "~~",
        "[link](https://example.com/a?b=c)",
        "[link](javascript:alert(1))",
        "<https://example.com>",
        "![image](https://example.com/i.png)",
        "| a | b |\n|:--|--:|\n| 1 | 2 |\n",
        "|",
        "  \n",
        "\\",
        "&amp;",
        "<script>alert(1)</script>",
        "<img src=x onerror=alert(1)>",
        "<a href=\"javascript:alert(1)\">x</a>",
        "<iframe src=\"https://example.com\"></iframe>",
        "<div onclick=\"x()\">",
        "</div>",
        "<!-- ",
        " -->",
    ];
    prop_oneof![
        6 => prop::sample::select(SYNTAX).prop_map(str::to_owned),
        3 => "[a-z]{1,8}",
        1 => "\\PC{0,6}",
    ]
}

fn pieces() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(piece(), 0..24)
}

/// The text `nodes` hold, in order.
fn text_of(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(text) | Node::Code(text) | Node::CodeBlock { text, .. } => {
                out.push_str(text);
            }
            Node::HardBreak | Node::Rule | Node::Task(_) => {}
            Node::Element { children, .. } => text_of(children, out),
        }
    }
}

/// The elements a message is made of: those `render` writes and no other.
const TAGS: &[&str] = &[
    "div",
    "p",
    "h1",
    "h2",
    "h3",
    "h4",
    "blockquote",
    "ol",
    "ul",
    "li",
    "em",
    "strong",
    "del",
    "a",
    "span",
    "table",
    "thead",
    "tr",
    "th",
    "td",
    "code",
    "pre",
    "br",
    "hr",
    "input",
    "button",
];

/// The attributes those elements carry. None of them loads or runs
/// anything: no `href`, no `src`, no `style`, no inner HTML.
const ATTRIBUTES: &[&str] = &[
    "class",
    "title",
    "type",
    "checked",
    "disabled",
    "start",
    "align",
    "data-lang",
];

/// Whether `node` is the copy button of a code block or part of its icon,
/// which the app draws and the message has no say in.
fn in_copy_button(dom: &Dom, copy: &[NodeRef], node: NodeRef) -> bool {
    copy.iter()
        .any(|button| *button == node || dom.find_in(*button, |_, inner| inner == node).len() == 1)
}

proptest! {
    #[test]
    fn nothing_in_a_message_is_live(pieces in pieces()) {
        let shown = Shown::of(&pieces.concat());
        let dom = &shown.screen.dom;
        let copy = dom.by_class("copy");
        for node in dom.elements() {
            if in_copy_button(dom, &copy, node) {
                continue;
            }
            let tag = dom.tag(node);
            prop_assert!(TAGS.contains(&tag), "<{}> in {}", tag, dom.html());
            for name in dom.attributes(node) {
                prop_assert!(
                    ATTRIBUTES.contains(&name),
                    "`{}` on <{}> in {}",
                    name,
                    tag,
                    dom.html()
                );
            }
            // A checkbox shows a task's state; the user cannot change it.
            if tag == "input" {
                prop_assert_eq!(dom.attribute(node, "type"), Some("checkbox"));
                prop_assert_eq!(dom.attribute(node, "disabled"), Some("true"));
            }
        }
    }

    #[test]
    fn shows_all_the_text_it_read(pieces in pieces()) {
        let source = pieces.concat();
        let shown = Shown::of(&source);
        let mut expected = String::new();
        text_of(&markdown::parse(&source), &mut expected);
        prop_assert_eq!(shown.screen.dom.text(shown.screen.dom.root()), expected);
    }

    #[test]
    fn a_streamed_message_reads_as_one_sent_whole(pieces in pieces()) {
        let mut streamed = Shown::new();
        let mut source = String::new();
        for piece in &pieces {
            source.push_str(piece);
            streamed.set(&source);
        }
        let whole = Shown::of(&source);
        prop_assert_eq!(streamed.screen.dom.html(), whole.screen.dom.html());
    }
}
