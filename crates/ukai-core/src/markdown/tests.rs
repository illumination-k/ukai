use rstest::rstest;

use super::*;

fn text(text: &str) -> Node {
    Node::Text(text.to_owned())
}

fn element(kind: Kind, children: Vec<Node>) -> Node {
    Node::Element { kind, children }
}

fn paragraph(children: Vec<Node>) -> Node {
    element(Kind::Paragraph, children)
}

#[test]
fn inline_styles_nest() {
    assert_eq!(
        parse("a **b _c_** `d` ~~e~~"),
        [paragraph(vec![
            text("a "),
            element(
                Kind::Strong,
                vec![text("b "), element(Kind::Emphasis, vec![text("c")])]
            ),
            text(" "),
            Node::Code("d".into()),
            text(" "),
            element(Kind::Strikethrough, vec![text("e")]),
        ])]
    );
}

#[test]
fn blocks() {
    assert_eq!(
        parse("# Title\n\n> quoted\n\n---\n\n1. one\n2. two\n"),
        [
            element(Kind::Heading(1), vec![text("Title")]),
            element(Kind::Quote, vec![paragraph(vec![text("quoted")])]),
            Node::Rule,
            element(
                Kind::List { start: Some(1) },
                vec![
                    element(Kind::Item, vec![text("one")]),
                    element(Kind::Item, vec![text("two")]),
                ]
            ),
        ]
    );
}

#[test]
fn soft_breaks_stay_in_one_text_node_and_hard_breaks_do_not() {
    assert_eq!(
        parse("a\nb  \nc"),
        [paragraph(vec![text("a\nb"), Node::HardBreak, text("c")])]
    );
}

#[rstest]
#[case::fenced("```rust title\nfn main() {}\n\n<b>x</b>\n```", Some("rust"))]
#[case::bare_fence("```\nfn main() {}\n\n<b>x</b>\n```", None)]
fn code_blocks_keep_their_text_verbatim(#[case] source: &str, #[case] lang: Option<&str>) {
    assert_eq!(
        parse(source),
        [Node::CodeBlock {
            lang: lang.map(str::to_owned),
            text: "fn main() {}\n\n<b>x</b>\n".into(),
        }]
    );
}

#[test]
fn an_unclosed_fence_while_streaming_runs_to_the_end() {
    assert_eq!(
        parse("```sh\nls"),
        [Node::CodeBlock {
            lang: Some("sh".into()),
            text: "ls".into(),
        }]
    );
}

#[rstest]
#[case::block("<script>alert(1)</script>\n", "<script>alert(1)</script>\n")]
#[case::inline("a <img src=x onerror=alert(1)> b", "a <img src=x onerror=alert(1)> b")]
fn raw_html_stays_text(#[case] source: &str, #[case] shown: &str) {
    let nodes = parse(source);
    let flat = match nodes.as_slice() {
        [Node::Text(text)] => text.clone(),
        [Node::Element { children, .. }] => match children.as_slice() {
            [Node::Text(text)] => text.clone(),
            other => panic!("unexpected children {other:?}"),
        },
        other => panic!("unexpected nodes {other:?}"),
    };
    assert_eq!(flat, shown);
}

#[rstest]
#[case::https("https://example.com/a", Some("https://example.com/a"))]
#[case::http("http://example.com", Some("http://example.com"))]
#[case::mailto("mailto:a@example.com", Some("mailto:a@example.com"))]
#[case::javascript("javascript:alert(1)", None)]
#[case::file("file:///etc/passwd", None)]
#[case::relative("docs/design.md", None)]
fn links_keep_only_safe_targets(#[case] url: &str, #[case] kept: Option<&str>) {
    assert_eq!(
        parse(&format!("[x]({url})")),
        [paragraph(vec![element(
            Kind::Link {
                url: kept.map(str::to_owned)
            },
            vec![text("x")]
        )])]
    );
}

#[test]
fn images_keep_only_their_alt_text() {
    assert_eq!(
        parse("![a plot](https://example.com/p.png)"),
        [paragraph(vec![element(Kind::Image, vec![text("a plot")])])]
    );
}

#[test]
fn tables_carry_alignment_per_cell() {
    let cell =
        |header, align, value: &str| element(Kind::TableCell { header, align }, vec![text(value)]);
    assert_eq!(
        parse("| a | b |\n|:--|--:|\n| 1 | 2 |\n"),
        [element(
            Kind::Table,
            vec![
                element(
                    Kind::TableHead,
                    vec![cell(true, Align::Left, "a"), cell(true, Align::Right, "b")]
                ),
                element(
                    Kind::TableRow,
                    vec![
                        cell(false, Align::Left, "1"),
                        cell(false, Align::Right, "2")
                    ]
                ),
            ]
        )]
    );
}

#[test]
fn task_lists() {
    assert_eq!(
        parse("- [x] done\n- [ ] todo\n"),
        [element(
            Kind::List { start: None },
            vec![
                element(Kind::Item, vec![Node::Task(true), text("done")]),
                element(Kind::Item, vec![Node::Task(false), text("todo")]),
            ]
        )]
    );
}

fn link(url: &str) -> Node {
    element(
        Kind::Link {
            url: Some(url.to_owned()),
        },
        vec![text(url)],
    )
}

#[rstest]
#[case::alone("https://example.com/a?b=1", vec![link("https://example.com/a?b=1")])]
#[case::http("see http://example.com", vec![text("see "), link("http://example.com")])]
#[case::trailing_punctuation(
    "at https://example.com/a.",
    vec![text("at "), link("https://example.com/a"), text(".")]
)]
#[case::unbalanced_paren(
    "(https://example.com/a)",
    vec![text("("), link("https://example.com/a"), text(")")]
)]
#[case::balanced_paren(
    "https://en.wikipedia.org/wiki/Rust_(language)",
    vec![link("https://en.wikipedia.org/wiki/Rust_(language)")]
)]
#[case::japanese_after(
    "https://example.comを見て",
    vec![link("https://example.com"), text("を見て")]
)]
#[case::two(
    "https://a.example, https://b.example",
    vec![link("https://a.example"), text(", "), link("https://b.example")]
)]
#[case::mid_word("xhttps://example.com", vec![text("xhttps://example.com")])]
#[case::scheme_only("https:// is a scheme", vec![text("https:// is a scheme")])]
#[case::other_scheme("ftp://example.com", vec![text("ftp://example.com")])]
fn bare_urls_become_links(#[case] source: &str, #[case] expected: Vec<Node>) {
    assert_eq!(parse(source), [paragraph(expected)]);
}

#[rstest]
#[case::inline_code("`https://example.com`", vec![paragraph(vec![Node::Code("https://example.com".into())])])]
#[case::code_block(
    "```\nhttps://example.com\n```",
    vec![Node::CodeBlock { lang: None, text: "https://example.com\n".into() }]
)]
#[case::link_text(
    "[https://a.example](https://b.example)",
    vec![paragraph(vec![element(
        Kind::Link { url: Some("https://b.example".into()) },
        vec![text("https://a.example")],
    )])]
)]
fn urls_in_code_and_links_are_left_alone(#[case] source: &str, #[case] expected: Vec<Node>) {
    assert_eq!(parse(source), expected);
}

#[test]
fn bare_urls_link_inside_nested_blocks() {
    assert_eq!(
        parse("- **https://example.com**"),
        [element(
            Kind::List { start: None },
            vec![element(
                Kind::Item,
                vec![element(Kind::Strong, vec![link("https://example.com")])]
            )]
        )]
    );
}
