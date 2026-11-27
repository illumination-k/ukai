//! Renders an agent's Markdown from the tree `ukai_core::markdown` parses.
//! Every node becomes an rsx! element, so text is escaped and nothing in the
//! source turns into live HTML.

use std::cell::RefCell;
use std::rc::Rc;

use dioxus::prelude::*;
use ukai_core::markdown::{self, Align, Kind, Node, Stream};

use crate::clipboard;
use crate::icons::{Icon, Svg};
use crate::report_pane::open_external;

/// A button that copies `text` and shows for a moment that it did.
#[component]
pub fn CopyButton(text: String) -> Element {
    let mut copied = use_signal(|| false);
    let copy = move |event: MouseEvent| {
        event.stop_propagation();
        if clipboard::copy(&text) {
            copied.set(true);
            spawn(async move {
                tokio::time::sleep(COPIED_FOR).await;
                copied.set(false);
            });
        }
    };
    rsx! {
        button {
            class: if copied() { "icon copy copied" } else { "icon copy" },
            title: if copied() { "Copied" } else { "Copy" },
            onclick: copy,
            Svg { icon: if copied() { Icon::Check } else { Icon::Copy } }
        }
    }
}

/// How long a copy button shows that it copied.
const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1500);

#[component]
pub fn Markdown(source: String) -> Element {
    // A message that streams in renders with every chunk, but all that
    // changes is its last block: the blocks before it are set aside in
    // parts, each parsed and drawn once, and only what follows them is
    // parsed and drawn again.
    let stream = use_hook(|| Rc::new(RefCell::new(Stream::default())));
    let (parts, nodes) = {
        let mut stream = stream.borrow_mut();
        let nodes = stream.parse(&source);
        (stream.parts().to_vec(), nodes)
    };
    rsx! {
        div { class: "md",
            for source in parts {
                Part { source }
            }
            {nodes.iter().map(render)}
        }
    }
}

/// Blocks of a message that stand, whatever is added after them: drawn
/// again only when given another source.
#[component]
fn Part(source: Rc<str>) -> Element {
    let nodes = markdown::parse(&source);
    rsx! {
        {nodes.iter().map(render)}
    }
}

fn render(node: &Node) -> Element {
    match node {
        Node::Text(text) => rsx! { "{text}" },
        Node::Code(code) => rsx! {
            code { "{code}" }
        },
        Node::CodeBlock { lang, text } => rsx! {
            div { class: "code-block",
                pre { "data-lang": lang.clone(),
                    code { "{text}" }
                }
                CopyButton { text: text.clone() }
            }
        },
        Node::HardBreak => rsx! {
            br {}
        },
        Node::Rule => rsx! {
            hr {}
        },
        Node::Task(checked) => rsx! {
            input { r#type: "checkbox", checked: *checked, disabled: true }
        },
        Node::Element { kind, children } => element(kind, children),
    }
}

fn element(kind: &Kind, children: &[Node]) -> Element {
    let inner = rsx! {
        {children.iter().map(render)}
    };
    match kind {
        Kind::Paragraph => rsx! {
            p { {inner} }
        },
        Kind::Heading(1) => rsx! {
            h1 { {inner} }
        },
        Kind::Heading(2) => rsx! {
            h2 { {inner} }
        },
        Kind::Heading(3) => rsx! {
            h3 { {inner} }
        },
        Kind::Heading(_) => rsx! {
            h4 { {inner} }
        },
        Kind::Quote => rsx! {
            blockquote { {inner} }
        },
        Kind::List { start: Some(start) } => rsx! {
            ol { start: "{start}", {inner} }
        },
        Kind::List { start: None } => rsx! {
            ul { {inner} }
        },
        Kind::Item => rsx! {
            li { {inner} }
        },
        Kind::Emphasis => rsx! {
            em { {inner} }
        },
        Kind::Strong => rsx! {
            strong { {inner} }
        },
        Kind::Strikethrough => rsx! {
            del { {inner} }
        },
        // No href: the app's web view must never navigate; the system
        // browser opens the link instead.
        Kind::Link { url: Some(url) } => {
            let target = url.clone();
            rsx! {
                a {
                    class: "link",
                    title: "{url}",
                    onclick: move |event| {
                        event.prevent_default();
                        open_external(&target);
                    },
                    {inner}
                }
            }
        }
        Kind::Link { url: None } => rsx! {
            span { {inner} }
        },
        Kind::Image => rsx! {
            span { class: "image-alt", title: "Image not loaded", {inner} }
        },
        Kind::Table => rsx! {
            div { class: "table-block",
                div { class: "table-wrap",
                    table { {inner} }
                }
                CopyButton { text: markdown::table_markdown(children) }
            }
        },
        Kind::TableHead => rsx! {
            thead {
                tr { {inner} }
            }
        },
        Kind::TableRow => rsx! {
            tr { {inner} }
        },
        Kind::TableCell { header, align } => {
            let align = match align {
                Align::None => "",
                Align::Left => "left",
                Align::Center => "center",
                Align::Right => "right",
            };
            if *header {
                rsx! {
                    th { "align": align, {inner} }
                }
            } else {
                rsx! {
                    td { "align": align, {inner} }
                }
            }
        }
    }
}
