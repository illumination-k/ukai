//! An agent's message on screen. A document written from a known structure
//! shows as that structure; whatever else an agent writes, nothing of it
//! turns into live HTML and none of its text is lost; and a message that
//! arrived in pieces reads as one that arrived whole.

use proptest::prelude::*;

use super::*;
use crate::testing::{Content, Dom, NodeRef, Screen};

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

// ---- documents of a known structure ----------------------------------------

/// A span of a line. Every `String` is a lowercase word, so nothing in it
/// is Markdown by accident.
#[derive(Debug, Clone)]
enum Inline {
    Text(String),
    Strong(String),
    Emphasis(String),
    Strike(String),
    Code(String),
    /// Link text and where it goes.
    Link(String, String),
    /// Alt text.
    Image(String),
}

#[derive(Debug, Clone)]
enum Block {
    /// A level from 1 to 6.
    Heading(u8, Vec<Inline>),
    Paragraph(Vec<Inline>),
    Bullets(Vec<Vec<Inline>>),
    /// The first item's number.
    Numbered(u64, Vec<Vec<Inline>>),
    Tasks(Vec<(bool, String)>),
    /// The language, if named, and the lines.
    Code(Option<String>, Vec<String>),
    Quote(Vec<Inline>),
    Rule,
    /// Each column's alignment and heading, then the rows.
    Table(Vec<(Align, String)>, Vec<Vec<String>>),
}

fn word() -> impl Strategy<Value = String> {
    "[a-z]{1,6}"
}

/// Where a link goes, and whether the app lets it be opened
/// (`ukai_core::markdown::LINK_SCHEMES`).
fn url() -> impl Strategy<Value = String> {
    (
        prop::sample::select(
            &[
                "https://example.com/",
                "http://example.com/",
                "mailto:",
                "javascript:",
                "file:///etc/",
                "ftp://example.com/",
            ][..],
        ),
        word(),
    )
        .prop_map(|(scheme, rest)| format!("{scheme}{rest}"))
}

fn opens(url: &str) -> bool {
    ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
}

fn inline() -> impl Strategy<Value = Inline> {
    prop_oneof![
        3 => word().prop_map(Inline::Text),
        1 => word().prop_map(Inline::Strong),
        1 => word().prop_map(Inline::Emphasis),
        1 => word().prop_map(Inline::Strike),
        1 => word().prop_map(Inline::Code),
        2 => (word(), url()).prop_map(|(text, url)| Inline::Link(text, url)),
        1 => word().prop_map(Inline::Image),
    ]
}

fn inlines() -> impl Strategy<Value = Vec<Inline>> {
    prop::collection::vec(inline(), 1..4)
}

fn align() -> impl Strategy<Value = Align> {
    prop_oneof![
        Just(Align::None),
        Just(Align::Left),
        Just(Align::Center),
        Just(Align::Right),
    ]
}

fn table() -> impl Strategy<Value = Block> {
    (1..4usize).prop_flat_map(|columns| {
        (
            prop::collection::vec((align(), word()), columns),
            prop::collection::vec(prop::collection::vec(word(), columns), 0..3),
        )
            .prop_map(|(head, rows)| Block::Table(head, rows))
    })
}

fn block() -> impl Strategy<Value = Block> {
    prop_oneof![
        2 => (1..=6u8, inlines()).prop_map(|(level, line)| Block::Heading(level, line)),
        3 => inlines().prop_map(Block::Paragraph),
        2 => prop::collection::vec(inlines(), 1..4).prop_map(Block::Bullets),
        2 => (1..50u64, prop::collection::vec(inlines(), 1..4))
            .prop_map(|(start, items)| Block::Numbered(start, items)),
        1 => prop::collection::vec((any::<bool>(), word()), 1..4).prop_map(Block::Tasks),
        2 => (prop::option::of(word()), prop::collection::vec(word(), 1..4))
            .prop_map(|(lang, lines)| Block::Code(lang, lines)),
        1 => inlines().prop_map(Block::Quote),
        1 => Just(Block::Rule),
        2 => table(),
    ]
}

/// Blocks to write one after another. Two lists of the same marker would
/// read as one list, so the second of such a pair is left out.
fn document() -> impl Strategy<Value = Vec<Block>> {
    fn marker(block: &Block) -> Option<char> {
        match block {
            Block::Bullets(_) | Block::Tasks(_) => Some('-'),
            Block::Numbered(..) => Some('.'),
            _ => None,
        }
    }
    prop::collection::vec(block(), 0..8).prop_map(|blocks| {
        let mut kept: Vec<Block> = Vec::new();
        for block in blocks {
            let repeats =
                marker(&block).is_some() && kept.last().and_then(marker) == marker(&block);
            if !repeats {
                kept.push(block);
            }
        }
        kept
    })
}

fn write_line(line: &[Inline]) -> String {
    let spans: Vec<String> = line
        .iter()
        .map(|inline| match inline {
            Inline::Text(text) => text.clone(),
            Inline::Strong(text) => format!("**{text}**"),
            Inline::Emphasis(text) => format!("*{text}*"),
            Inline::Strike(text) => format!("~~{text}~~"),
            Inline::Code(text) => format!("`{text}`"),
            Inline::Link(text, url) => format!("[{text}]({url})"),
            Inline::Image(alt) => format!("![{alt}](https://example.com/{alt}.png)"),
        })
        .collect();
    spans.join(" ")
}

/// The Markdown an agent would write for `blocks`.
fn write(blocks: &[Block]) -> String {
    let written: Vec<String> = blocks
        .iter()
        .map(|block| match block {
            Block::Heading(level, line) => {
                format!("{} {}", "#".repeat(usize::from(*level)), write_line(line))
            }
            Block::Paragraph(line) => write_line(line),
            Block::Bullets(items) => items
                .iter()
                .map(|item| format!("- {}", write_line(item)))
                .collect::<Vec<_>>()
                .join("\n"),
            Block::Numbered(start, items) => items
                .iter()
                .zip(*start..)
                .map(|(item, number)| format!("{number}. {}", write_line(item)))
                .collect::<Vec<_>>()
                .join("\n"),
            Block::Tasks(items) => items
                .iter()
                .map(|(done, text)| format!("- [{}] {text}", if *done { 'x' } else { ' ' }))
                .collect::<Vec<_>>()
                .join("\n"),
            Block::Code(lang, lines) => format!(
                "```{}\n{}\n```",
                lang.as_deref().unwrap_or_default(),
                lines.join("\n")
            ),
            Block::Quote(line) => format!("> {}", write_line(line)),
            Block::Rule => "---".to_owned(),
            Block::Table(head, rows) => {
                let row = |cells: Vec<&str>| format!("| {} |", cells.join(" | "));
                let mut lines = vec![
                    row(head.iter().map(|(_, name)| name.as_str()).collect()),
                    row(head
                        .iter()
                        .map(|(align, _)| match align {
                            Align::None => "---",
                            Align::Left => ":--",
                            Align::Center => ":-:",
                            Align::Right => "--:",
                        })
                        .collect()),
                ];
                lines.extend(
                    rows.iter()
                        .map(|cells| row(cells.iter().map(String::as_str).collect())),
                );
                lines.join("\n")
            }
        })
        .collect();
    written.join("\n\n")
}

/// What a reader sees of a message: its elements, the attributes that say
/// something to them, and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Text(String),
    Element(&'static str, Vec<(&'static str, String)>, Vec<Seen>),
}

fn element<const N: usize>(
    tag: &'static str,
    attributes: [(&'static str, &str); N],
    children: Vec<Seen>,
) -> Seen {
    let attributes = attributes
        .into_iter()
        .map(|(name, value)| (name, value.to_owned()))
        .collect();
    Seen::Element(tag, attributes, joined(children))
}

fn text(text: &str) -> Seen {
    Seen::Text(text.to_owned())
}

/// `nodes` with each run of text as one.
fn joined(nodes: Vec<Seen>) -> Vec<Seen> {
    let mut out: Vec<Seen> = Vec::new();
    for node in nodes {
        match (out.last_mut(), node) {
            (Some(Seen::Text(before)), Seen::Text(more)) => before.push_str(&more),
            (_, node) => out.push(node),
        }
    }
    out
}

fn expected_line(line: &[Inline]) -> Vec<Seen> {
    let mut out = Vec::new();
    for (index, inline) in line.iter().enumerate() {
        if index > 0 {
            out.push(text(" "));
        }
        out.push(match inline {
            Inline::Text(word) => text(word),
            Inline::Strong(word) => element("strong", [], vec![text(word)]),
            Inline::Emphasis(word) => element("em", [], vec![text(word)]),
            Inline::Strike(word) => element("del", [], vec![text(word)]),
            Inline::Code(word) => element("code", [], vec![text(word)]),
            // A link the app would open says where it goes; one it would
            // not is only its text.
            Inline::Link(word, url) if opens(url) => {
                element("a", [("title", url)], vec![text(word)])
            }
            Inline::Link(word, _) => element("span", [], vec![text(word)]),
            // An image is never loaded: its alt text stands for it.
            Inline::Image(alt) => element(
                "span",
                [("class", "image-alt"), ("title", "Image not loaded")],
                vec![text(alt)],
            ),
        });
    }
    out
}

/// What `blocks` should look like, written from what Markdown means and
/// what the app promises of it, not from how the app renders.
fn expected(blocks: &[Block]) -> Vec<Seen> {
    let items = |items: &[Vec<Inline>]| {
        items
            .iter()
            .map(|item| element("li", [], expected_line(item)))
            .collect()
    };
    let cell = |tag: &'static str, align: &Align, word: &str| {
        let align = match align {
            Align::None => None,
            Align::Left => Some("left"),
            Align::Center => Some("center"),
            Align::Right => Some("right"),
        };
        match align {
            Some(align) => element(tag, [("align", align)], vec![text(word)]),
            None => element(tag, [], vec![text(word)]),
        }
    };
    let blocks = blocks
        .iter()
        .map(|block| match block {
            // Levels below the fourth all show as the fourth.
            Block::Heading(level, line) => {
                let tag = match level {
                    1 => "h1",
                    2 => "h2",
                    3 => "h3",
                    _ => "h4",
                };
                element(tag, [], expected_line(line))
            }
            Block::Paragraph(line) => element("p", [], expected_line(line)),
            Block::Bullets(list) => element("ul", [], items(list)),
            Block::Numbered(start, list) => {
                element("ol", [("start", &start.to_string())], items(list))
            }
            Block::Tasks(list) => element(
                "ul",
                [],
                list.iter()
                    .map(|(done, word)| {
                        let checkbox =
                            element("input", [("checked", &done.to_string())], Vec::new());
                        element("li", [], vec![checkbox, text(word)])
                    })
                    .collect(),
            ),
            Block::Code(lang, lines) => {
                let code = element("code", [], vec![text(&format!("{}\n", lines.join("\n")))]);
                match lang {
                    Some(lang) => element("pre", [("data-lang", lang)], vec![code]),
                    None => element("pre", [], vec![code]),
                }
            }
            Block::Quote(line) => element(
                "blockquote",
                [],
                vec![element("p", [], expected_line(line))],
            ),
            Block::Rule => element("hr", [], Vec::new()),
            Block::Table(head, rows) => {
                let heading = head
                    .iter()
                    .map(|(align, name)| cell("th", align, name))
                    .collect();
                let mut children = vec![element("thead", [], vec![element("tr", [], heading)])];
                children.extend(rows.iter().map(|row| {
                    let cells = row
                        .iter()
                        .zip(head)
                        .map(|(word, (align, _))| cell("td", align, word))
                        .collect();
                    element("tr", [], cells)
                }));
                element("table", [], children)
            }
        })
        .collect();
    joined(blocks)
}

/// The attributes of `tag` that say something to the reader. Classes style
/// what the tag already says, except on a `<span>`, which says nothing.
fn telling(tag: &str) -> &'static [&'static str] {
    match tag {
        "ol" => &["start"],
        "th" | "td" => &["align"],
        "a" => &["title"],
        "span" => &["class", "title"],
        "pre" => &["data-lang"],
        "input" => &["checked"],
        _ => &[],
    }
}

/// What `node` shows. A `<div>` only groups, and the copy button of a code
/// block is the app's own, so neither is part of the message.
fn seen(dom: &Dom, node: NodeRef) -> Vec<Seen> {
    let mut out = Vec::new();
    for content in dom.content(node) {
        match content {
            Content::Text(text) => out.push(Seen::Text(text)),
            Content::Element(child) if dom.has_class(child, "copy") => {}
            Content::Element(child) if dom.tag(child) == "div" => out.extend(seen(dom, child)),
            Content::Element(child) => {
                let tag = dom.tag(child);
                let attributes = telling(tag)
                    .iter()
                    .filter_map(|name| {
                        dom.attribute(child, name)
                            .filter(|value| !value.is_empty())
                            .map(|value| (*name, value.to_owned()))
                    })
                    .collect();
                out.push(Seen::Element(tag, attributes, seen(dom, child)));
            }
        }
    }
    joined(out)
}

// ---- anything an agent might write -----------------------------------------

/// Pieces of Markdown, of HTML an agent might pass along or be tricked into
/// writing, and of anything else, to be joined in any order. Most of what
/// they make is not well-formed; what a well-formed document shows is
/// [`a_document_shows_as_written`]'s to check.
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

/// A message: a well-formed document, pieces in any order, or one after
/// the other.
fn message() -> impl Strategy<Value = String> {
    let pieces = || prop::collection::vec(piece(), 0..24).prop_map(|pieces| pieces.concat());
    prop_oneof![
        document().prop_map(|blocks| write(&blocks)),
        pieces(),
        (document(), pieces())
            .prop_map(|(blocks, pieces)| format!("{}\n\n{pieces}", write(&blocks))),
    ]
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

/// The elements a message is made of.
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

/// The elements of a message, without the copy buttons of its code blocks
/// and their icons, which the app draws and the message has no say in.
fn message_elements(dom: &Dom) -> Vec<NodeRef> {
    let mut own: Vec<NodeRef> = dom.by_class("copy");
    for button in own.clone() {
        own.extend(dom.find_in(button, |_, _| true));
    }
    dom.elements()
        .into_iter()
        .filter(|node| !own.contains(node))
        .collect()
}

proptest! {
    #[test]
    fn a_document_shows_as_written(blocks in document()) {
        let source = write(&blocks);
        let shown = Shown::of(&source);
        let dom = &shown.screen.dom;
        prop_assert_eq!(seen(dom, dom.root()), expected(&blocks), "of {:?}", source);
    }

    #[test]
    fn nothing_in_a_message_is_live(source in message()) {
        let shown = Shown::of(&source);
        let dom = &shown.screen.dom;
        for node in message_elements(dom) {
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

    /// Against the parser's own reading, so this holds the renderer alone
    /// to it: what the parser drops is for `ukai_core::markdown`'s tests.
    #[test]
    fn shows_all_the_text_it_read(source in message()) {
        let shown = Shown::of(&source);
        let mut expected = String::new();
        text_of(&markdown::parse(&source), &mut expected);
        // The copy buttons hold no text, so all of it is the message's.
        let shown_text = shown.screen.dom.text(shown.screen.dom.root());
        prop_assert_eq!(shown_text, expected);
    }

    #[test]
    fn a_streamed_message_reads_as_one_sent_whole(
        source in message(),
        cuts in prop::collection::vec(any::<prop::sample::Index>(), 0..12),
    ) {
        let ends: Vec<usize> = source
            .char_indices()
            .map(|(at, _)| at)
            .chain([source.len()])
            .collect();
        let mut cuts: Vec<usize> = cuts.iter().map(|cut| ends[cut.index(ends.len())]).collect();
        cuts.sort_unstable();
        cuts.push(source.len());

        let mut streamed = Shown::new();
        for cut in cuts {
            streamed.set(&source[..cut]);
        }
        let whole = Shown::of(&source);
        prop_assert_eq!(streamed.screen.dom.html(), whole.screen.dom.html());
    }
}
