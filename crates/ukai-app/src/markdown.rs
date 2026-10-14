//! Renders an agent's Markdown from the tree `ukai_core::markdown` parses.
//! Every node becomes an rsx! element, so text is escaped and nothing in the
//! source turns into live HTML.

use dioxus::prelude::*;
use ukai_core::markdown::{self, Align, Kind, Node};

use crate::report_pane::open_external;

#[component]
pub fn Markdown(source: String) -> Element {
    let nodes = markdown::parse(&source);
    rsx! {
        div { class: "md",
            for (index, node) in nodes.iter().enumerate() {
                Fragment { key: "{index}", {render(node)} }
            }
        }
    }
}

fn render(node: &Node) -> Element {
    match node {
        Node::Text(text) => rsx! { "{text}" },
        Node::Code(code) => rsx! {
            code { "{code}" }
        },
        Node::CodeBlock { lang, text } => rsx! {
            pre { "data-lang": lang.clone(),
                code { "{text}" }
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
        for (index, child) in children.iter().enumerate() {
            Fragment { key: "{index}", {render(child)} }
        }
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
            div { class: "table-wrap",
                table { {inner} }
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
