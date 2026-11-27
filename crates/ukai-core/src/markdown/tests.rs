use proptest::prelude::*;
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
fn tables_copy_as_markdown() {
    let [Node::Element { children, .. }] = &parse(concat!(
        "| name | note | n |\n",
        "|:--|:-:|--:|\n",
        "| **a** _b_ ~~c~~ | [link](https://x.test) https://y.test |  |\n",
        "| `a\\|b` ``c`d`` | x \\| y |  |\n",
    ))[..] else {
        panic!("not one table");
    };
    assert_eq!(
        table_markdown(children),
        concat!(
            "| name | note | n |\n",
            "| :-- | :-: | --: |\n",
            "| **a** *b* ~~c~~ | [link](https://x.test) https://y.test |  |\n",
            "| `a\\|b` ``c`d`` | x \\| y |  |\n",
        )
    );
}

#[test]
fn a_copied_table_parses_back_to_the_same_table() {
    let source = "| a | b |\n|---|--:|\n| **x** `y\\|z` | [l](https://x.test) |\n";
    let [Node::Element { children, .. }] = &parse(source)[..] else {
        panic!("not one table");
    };
    assert_eq!(parse(&table_markdown(children)), parse(source));
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

/// What `stream` makes of `source`: the nodes of its parts, then the rest.
fn through(stream: &mut Stream, source: &str) -> Vec<Node> {
    let rest = stream.parse(source);
    let mut nodes: Vec<Node> = stream.parts().iter().flat_map(|part| parse(part)).collect();
    nodes.extend(rest);
    nodes
}

/// A source of `blocks` paragraphs.
fn paragraphs(blocks: usize) -> String {
    (0..blocks)
        .map(|block| format!("paragraph {block}\n\n"))
        .collect()
}

#[test]
fn a_stream_sets_aside_the_blocks_that_stand() {
    let mut stream = Stream::default();
    let mut source = paragraphs(PART_BLOCKS);
    // The last block may still grow, so it is none of a part's.
    assert_eq!(stream.parse(&source).len(), PART_BLOCKS);
    assert!(stream.parts().is_empty());

    source.push_str("- one\n");
    assert_eq!(stream.parse(&source), parse("- one\n"));
    let parts = stream.parts().to_vec();
    assert_eq!(parts, [paragraphs(PART_BLOCKS).into()]);

    // Nor are the blocks before the last, while they are too few.
    source.push_str("- two\n\nsecond");
    assert_eq!(stream.parse(&source), parse("- one\n- two\n\nsecond"));
    assert!(Rc::ptr_eq(&stream.parts()[0], &parts[0]));
    assert_eq!(stream.parts().len(), 1);
}

#[rstest]
// A reference defined later turns earlier text into a link.
#[case::reference("see [docs]\n\n", "[docs]: https://example.com\n")]
// Raw HTML is text right under the root, which joins the text next to it.
#[case::html("<div>\n\n<span>\n\n", "<p>\n")]
// A fence starts a block right under a paragraph, until its line goes on.
#[case::no_blank_line("two\n```", " not a fence```")]
// A number that becomes `2.` joins the list above, past the blank line.
#[case::list_item("1. a\n\n2", ". b")]
fn a_stream_keeps_together_what_cannot_be_parsed_in_parts(#[case] first: &str, #[case] more: &str) {
    let mut stream = Stream::default();
    let before = paragraphs(PART_BLOCKS);
    for source in [format!("{before}{first}"), format!("{before}{first}{more}")] {
        assert_eq!(through(&mut stream, &source), parse(&source), "{source:?}");
    }
}

#[test]
fn a_stream_parses_another_source_anew() {
    let mut stream = Stream::default();
    stream.parse(&format!("{}last", paragraphs(PART_BLOCKS)));
    assert_eq!(stream.parts().len(), 1);

    let other = format!("{}    code\n\n    more code", paragraphs(PART_BLOCKS - 1));
    assert_eq!(through(&mut stream, &other), parse(&other));
}

/// Pieces that between them start, continue, and end every kind of block.
fn piece() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec![
        "word",
        " ",
        "\n",
        "\n\n",
        "\r\n",
        "1",
        "2",
        ".",
        ")",
        "-",
        "+",
        "=",
        "#",
        ">",
        "<",
        ":",
        "    ",
        "  ",
        "\t",
        "- ",
        "* ",
        "1. ",
        "2) ",
        "# ",
        "## ",
        "> ",
        "---",
        "===",
        "```",
        "```rust",
        "~~~",
        "|",
        "| a | b |",
        "|---|---|",
        "*",
        "**",
        "_",
        "`",
        "~~",
        "[x]",
        "[ ] ",
        "[docs]",
        "[docs]: https://example.com",
        "[a](https://a.b)",
        "![alt](x.png)",
        "https://example.com/a",
        "<div>",
        "</div>",
        "<!--",
        "-->",
        "<pre>",
        "</pre>",
        "<br>",
        "\\",
        "&amp;",
        "é",
        "日本語",
    ])
}

proptest! {
    /// However a source arrives, what a stream makes of it is what parsing
    /// it whole does.
    #[test]
    fn a_stream_parses_as_the_whole_source_does(
        chunks in prop::collection::vec(prop::collection::vec(piece(), 0..8), 1..24),
        part_blocks in 1..4_usize,
    ) {
        let mut stream = Stream { part_blocks, ..Stream::default() };
        let mut source = String::new();
        for chunk in chunks {
            source.extend(chunk);
            prop_assert_eq!(through(&mut stream, &source), parse(&source), "after {:?}", source);
        }
    }
}
