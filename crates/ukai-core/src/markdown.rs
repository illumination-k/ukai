//! Markdown in agent messages, parsed into a tree the app renders itself.
//!
//! Agent text is untrusted, and the transcript lives in the app's web view,
//! whose IPC bridge any script there can reach. So nothing here becomes HTML:
//! raw HTML in the source stays literal text, images show their alt text
//! instead of loading, and a link keeps its target only for the schemes the
//! app hands to the system browser. Bare `http(s)://` URLs become links too.

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
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder::default();
    for event in Parser::new_ext(source, options) {
        builder.event(event);
    }
    builder.finish()
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

    fn finish(mut self) -> Vec<Node> {
        while !self.open.is_empty() {
            self.close();
        }
        autolink(self.root)
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
