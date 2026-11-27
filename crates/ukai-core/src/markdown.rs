//! Markdown in agent messages, parsed into a tree the app renders itself.
//!
//! Agent text is untrusted, and the transcript lives in the app's web view,
//! whose IPC bridge any script there can reach. So nothing here becomes HTML:
//! raw HTML in the source stays literal text, images show their alt text
//! instead of loading, and a link keeps its target only for the schemes the
//! app hands to the system browser. Bare `http(s)://` URLs become links too.

use std::rc::Rc;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// Schemes a link may open with; anything else renders as plain text.
pub const LINK_SCHEMES: [&str; 3] = ["https://", "http://", "mailto:"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    Text(String),
    /// Inline code.
    Code(String),
    CodeBlock {
        lang: Option<String>,
        text: String,
    },
    HardBreak,
    Rule,
    /// A task list item's checkbox.
    Task(bool),
    Element {
        kind: Kind,
        children: Vec<Node>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Paragraph,
    /// 1 to 6.
    Heading(u8),
    Quote,
    /// `start` is the first number of an ordered list.
    List {
        start: Option<u64>,
    },
    Item,
    Emphasis,
    Strong,
    Strikethrough,
    /// `url` is `None` unless it has one of the [`LINK_SCHEMES`].
    Link {
        url: Option<String>,
    },
    /// Shows its alt text; the image is never loaded.
    Image,
    Table,
    TableHead,
    TableRow,
    TableCell {
        header: bool,
        align: Align,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    None,
    Left,
    Center,
    Right,
}

/// What an open tag becomes when it closes.
enum Frame {
    Element(Kind),
    CodeBlock(Option<String>),
    /// A tag with nothing to show of its own (e.g. a footnote definition):
    /// its children go to the parent.
    Transparent,
}

/// Parses CommonMark with tables, strikethrough, and task lists.
pub fn parse(source: &str) -> Vec<Node> {
    autolink(Blocks::of(source).root)
}

/// The top-level nodes of a source, before [`autolink`].
struct Blocks {
    root: Vec<Node>,
    /// Where the last top-level block that parses the same on its own
    /// starts, whatever is added after it: its byte offset, and how many of
    /// `root` come before it.
    last: Option<(usize, usize)>,
    /// The source defines link references, which a link anywhere in it may
    /// use: no part of it parses the same on its own.
    references: bool,
}

impl Blocks {
    fn of(source: &str) -> Self {
        let options =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        let parser = Parser::new_ext(source, options);
        let references = parser.reference_definitions().iter().next().is_some();
        let mut builder = Builder::default();
        // Where each top-level block starts, as `last` has it.
        let mut starts = Vec::new();
        for (event, range) in parser.into_offset_iter() {
            if builder.open.is_empty() {
                // From the start of its line: its indentation is part of it.
                let start = source[..range.start].rfind('\n').map_or(0, |end| end + 1);
                starts.push((start, builder.root.len()));
            }
            builder.event(event);
        }
        while !builder.open.is_empty() {
            builder.close();
        }
        let root = builder.root;
        // A block parses the same on its own only if nothing before it can
        // take it in, whatever is added to it:
        // - a blank line comes before it, or it could turn out to continue
        //   the paragraph above (as a code fence does once its line goes on);
        // - no list comes right before it, which a number that becomes
        //   `2.` would join as an item, even past a blank line;
        // - no text right under the root is next to it (a block of raw HTML
        //   is such), which joins the text beside it into one node.
        let is_text = |index: usize| matches!(root.get(index), Some(Node::Text(_)));
        let is_list = |index: usize| {
            matches!(
                root.get(index),
                Some(Node::Element {
                    kind: Kind::List { .. },
                    ..
                })
            )
        };
        let after_blank = |start: usize| {
            source[..start].strip_suffix('\n').is_none_or(|before| {
                before
                    .rsplit('\n')
                    .next()
                    .is_some_and(|line| line.trim().is_empty())
            })
        };
        let last = starts.into_iter().rev().find(|&(start, index)| {
            after_blank(start)
                && !is_text(index)
                && !(index > 0 && (is_text(index - 1) || is_list(index - 1)))
        });
        Self {
            root,
            last,
            references,
        }
    }
}

/// Parses a message again and again as it streams in, without parsing all of
/// it each time: text added at the end changes only the last block, so the
/// blocks before it are set aside in parts that stand, each to be parsed
/// (and drawn) once, and only what follows them is parsed again.
#[derive(Debug)]
pub struct Stream {
    parts: Vec<Rc<str>>,
    /// Blocks a part has, or more.
    part_blocks: usize,
}

/// Blocks a part of a [`Stream`] has, or more: enough that a long message
/// is few parts, and few enough that what follows them stays short.
const PART_BLOCKS: usize = 8;

impl Default for Stream {
    fn default() -> Self {
        Self {
            parts: Vec::new(),
            part_blocks: PART_BLOCKS,
        }
    }
}

impl Stream {
    /// The start of the source last parsed, in parts that each parse on
    /// their own to the nodes they have in the whole: [`parse`] of each, one
    /// after the other, then what [`Self::parse`] returned, is [`parse`] of
    /// the source. A part stays the same [`Rc`] while the source only grows.
    pub fn parts(&self) -> &[Rc<str>] {
        &self.parts
    }

    /// Parses `source` and returns the nodes that follow [`Self::parts`].
    /// Any source will do; one that only grew since the last call keeps the
    /// parts it had.
    pub fn parse(&mut self, source: &str) -> Vec<Node> {
        let mut rest = source;
        for part in &self.parts {
            match rest.strip_prefix(&**part) {
                Some(after) => rest = after,
                None => {
                    // Another source: what stood in the last one, whatever
                    // was added to it, need not stand in this one.
                    self.parts.clear();
                    rest = source;
                    break;
                }
            }
        }
        let mut blocks = Blocks::of(rest);
        if blocks.references {
            if !self.parts.is_empty() {
                self.parts.clear();
                blocks = Blocks::of(source);
            }
            return autolink(blocks.root);
        }
        if let Some((start, index)) = blocks.last
            && index >= self.part_blocks
        {
            self.parts.push(rest[..start].into());
            blocks.root.drain(..index);
        }
        autolink(blocks.root)
    }
}

#[derive(Default)]
struct Builder {
    root: Vec<Node>,
    open: Vec<(Frame, Vec<Node>)>,
    /// Column alignments of the table being built, and where its cells are.
    table: Vec<Align>,
    in_head: bool,
    column: usize,
}

impl Builder {
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => {
                let frame = self.frame(tag);
                self.open.push((frame, Vec::new()));
            }
            Event::End(end) => {
                match end {
                    TagEnd::TableHead => self.in_head = false,
                    TagEnd::TableCell => self.column += 1,
                    _ => {}
                }
                self.close();
            }
            Event::Text(text)
            | Event::Html(text)
            | Event::InlineHtml(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text)
            | Event::FootnoteReference(text) => self.text(&text),
            Event::Code(code) => self.push(Node::Code(code.into_string())),
            Event::SoftBreak => self.text("\n"),
            Event::HardBreak => self.push(Node::HardBreak),
            Event::Rule => self.push(Node::Rule),
            Event::TaskListMarker(checked) => self.push(Node::Task(checked)),
        }
    }

    fn frame(&mut self, tag: Tag<'_>) -> Frame {
        let kind = match tag {
            Tag::Paragraph => Kind::Paragraph,
            Tag::Heading { level, .. } => Kind::Heading(level as u8),
            Tag::BlockQuote(_) => Kind::Quote,
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split_whitespace()
                        .next()
                        .map(str::to_owned)
                        .filter(|lang| !lang.is_empty()),
                    CodeBlockKind::Indented => None,
                };
                return Frame::CodeBlock(lang);
            }
            Tag::List(start) => Kind::List { start },
            Tag::Item => Kind::Item,
            Tag::Emphasis => Kind::Emphasis,
            Tag::Strong => Kind::Strong,
            Tag::Strikethrough => Kind::Strikethrough,
            Tag::Link { dest_url, .. } => Kind::Link {
                url: LINK_SCHEMES
                    .iter()
                    .any(|scheme| dest_url.starts_with(scheme))
                    .then(|| dest_url.into_string()),
            },
            Tag::Image { .. } => Kind::Image,
            Tag::Table(alignments) => {
                self.table = alignments.into_iter().map(Align::from).collect();
                Kind::Table
            }
            Tag::TableHead => {
                self.in_head = true;
                self.column = 0;
                Kind::TableHead
            }
            Tag::TableRow => {
                self.column = 0;
                Kind::TableRow
            }
            Tag::TableCell => Kind::TableCell {
                header: self.in_head,
                align: self.table.get(self.column).copied().unwrap_or(Align::None),
            },
            _ => return Frame::Transparent,
        };
        Frame::Element(kind)
    }

    fn close(&mut self) {
        let Some((frame, children)) = self.open.pop() else {
            return;
        };
        match frame {
            Frame::Element(kind) => self.push(Node::Element { kind, children }),
            Frame::CodeBlock(lang) => {
                let text = children
                    .into_iter()
                    .filter_map(|node| match node {
                        Node::Text(text) => Some(text),
                        _ => None,
                    })
                    .collect();
                self.push(Node::CodeBlock { lang, text });
            }
            Frame::Transparent => {
                for child in children {
                    self.push(child);
                }
            }
        }
    }

    fn children(&mut self) -> &mut Vec<Node> {
        match self.open.last_mut() {
            Some((_, children)) => children,
            None => &mut self.root,
        }
    }

    fn push(&mut self, node: Node) {
        if let Node::Text(text) = &node {
            self.text(text);
            return;
        }
        self.children().push(node);
    }

    /// Appends to the previous text node so a run of text stays one node.
    fn text(&mut self, text: &str) {
        let children = self.children();
        if let Some(Node::Text(last)) = children.last_mut() {
            last.push_str(text);
        } else {
            children.push(Node::Text(text.to_owned()));
        }
    }
}

/// Turns bare `http(s)://` URLs in text into links, as GitHub does. Text
/// already inside a link, or an image's alt text, stays as it is.
fn autolink(nodes: Vec<Node>) -> Vec<Node> {
    let mut linked = Vec::with_capacity(nodes.len());
    for node in nodes {
        match node {
            Node::Text(text) => split_urls(&text, &mut linked),
            Node::Element {
                kind: kind @ (Kind::Link { .. } | Kind::Image),
                children,
            } => linked.push(Node::Element { kind, children }),
            Node::Element { kind, children } => linked.push(Node::Element {
                kind,
                children: autolink(children),
            }),
            other => linked.push(other),
        }
    }
    linked
}

fn split_urls(text: &str, out: &mut Vec<Node>) {
    let mut rest = text;
    while let Some((start, end)) = find_url(rest) {
        if start > 0 {
            out.push(Node::Text(rest[..start].to_owned()));
        }
        let url = &rest[start..end];
        out.push(Node::Element {
            kind: Kind::Link {
                url: Some(url.to_owned()),
            },
            children: vec![Node::Text(url.to_owned())],
        });
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        out.push(Node::Text(rest.to_owned()));
    }
}

/// The byte range of the first bare URL in `text`.
fn find_url(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(offset) = text[from..].find("http") {
        let start = from + offset;
        from = start + "http".len();
        let rest = &text[start..];
        let Some(scheme) = ["https://", "http://"]
            .into_iter()
            .find(|scheme| rest.starts_with(scheme))
        else {
            continue;
        };
        // Not the middle of a word, as in `xhttp://`.
        if text[..start]
            .chars()
            .next_back()
            .is_some_and(char::is_alphanumeric)
        {
            continue;
        }
        let len = url_len(rest);
        if len > scheme.len() {
            return Some((start, start + len));
        }
    }
    None
}

/// How much of `text`, which starts with a scheme, is the URL: up to
/// whitespace, `<`, or a non-ASCII character (so a URL directly followed by
/// Japanese ends there), less trailing punctuation and an unbalanced `)`.
fn url_len(text: &str) -> usize {
    let mut end = text
        .find(|c: char| {
            !c.is_ascii() || c.is_ascii_whitespace() || c.is_ascii_control() || c == '<'
        })
        .unwrap_or(text.len());
    loop {
        let url = &text[..end];
        match url.chars().next_back() {
            Some('?' | '!' | '.' | ',' | ':' | ';' | '*' | '_' | '~' | '\'' | '"') => end -= 1,
            Some(')') if url.matches(')').count() > url.matches('(').count() => end -= 1,
            _ => return end,
        }
    }
}

/// A table as GitHub-flavored Markdown, header, alignment row, and body,
/// for pasting into another chat or document. `children` are a
/// [`Kind::Table`]'s. Inline styles, code, and links are written back as
/// Markdown; `|` is escaped and line breaks become spaces, so each row stays
/// one line.
pub fn table_markdown(children: &[Node]) -> String {
    let mut markdown = String::new();
    for row in children {
        let Node::Element {
            kind,
            children: cells,
        } = row
        else {
            continue;
        };
        if !matches!(kind, Kind::TableHead | Kind::TableRow) {
            continue;
        }
        markdown.push('|');
        for cell in cells {
            let mut text = String::new();
            inline_markdown(std::slice::from_ref(cell), &mut text);
            let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
            markdown.push(' ');
            markdown.push_str(&text);
            markdown.push_str(" |");
        }
        markdown.push('\n');
        if *kind == Kind::TableHead {
            markdown.push('|');
            for cell in cells {
                let align = match cell {
                    Node::Element {
                        kind: Kind::TableCell { align, .. },
                        ..
                    } => *align,
                    _ => Align::None,
                };
                markdown.push_str(match align {
                    Align::None => " --- |",
                    Align::Left => " :-- |",
                    Align::Center => " :-: |",
                    Align::Right => " --: |",
                });
            }
            markdown.push('\n');
        }
    }
    markdown
}

/// `nodes`, which sit inside a table cell, written back as Markdown.
fn inline_markdown(nodes: &[Node], out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(text) => out.push_str(&text.replace('|', "\\|")),
            Node::Code(code) => {
                // A fence longer than any run of backticks inside.
                let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
                let fence = "`".repeat(longest + 1);
                let pad = if code.starts_with('`') || code.ends_with('`') {
                    " "
                } else {
                    ""
                };
                out.push_str(&format!(
                    "{fence}{pad}{}{pad}{fence}",
                    code.replace('|', "\\|")
                ));
            }
            Node::CodeBlock { text, .. } => out.push_str(&text.replace('|', "\\|")),
            Node::HardBreak => out.push(' '),
            Node::Rule => {}
            Node::Task(checked) => out.push_str(if *checked { "[x] " } else { "[ ] " }),
            Node::Element { kind, children } => {
                let wrap = |out: &mut String, mark: &str| {
                    out.push_str(mark);
                    inline_markdown(children, out);
                    out.push_str(mark);
                };
                match kind {
                    Kind::Emphasis => wrap(out, "*"),
                    Kind::Strong => wrap(out, "**"),
                    Kind::Strikethrough => wrap(out, "~~"),
                    // A bare URL the parser linked goes back as it was.
                    Kind::Link { url: Some(url) } if matches!(&children[..], [Node::Text(text)] if text == url) =>
                    {
                        out.push_str(url);
                    }
                    Kind::Link { url: Some(url) } => {
                        out.push('[');
                        inline_markdown(children, out);
                        out.push_str("](");
                        out.push_str(url);
                        out.push(')');
                    }
                    _ => inline_markdown(children, out),
                }
            }
        }
    }
}

impl From<Alignment> for Align {
    fn from(alignment: Alignment) -> Self {
        match alignment {
            Alignment::None => Self::None,
            Alignment::Left => Self::Left,
            Alignment::Center => Self::Center,
            Alignment::Right => Self::Right,
        }
    }
}

#[cfg(test)]
mod tests;
