//! An agent's review of a checkout's diff ([`ukai_acp::review`]): what it may
//! write, how a new review replaces the last one, the threads of replies on
//! each annotation, where on the diff an annotation shows, and which ones a
//! filter lets through. The relay's MCP server and the diff pane share it.

use ukai_acp::review::{Annotation, Author, Priority, Reply, Review};
use ukai_config::is_tag;

use crate::diff::FileChange;
use crate::knowledge::invisible;
use crate::review::{DiffLine, LineKind, ReviewComment};

/// Longest summary, in bytes.
pub const SUMMARY_MAX: usize = 16 * 1024;
/// Longest annotation or reply, in bytes.
pub const BODY_MAX: usize = 8 * 1024;
/// Most annotations in one review: past that it is not a review the user
/// can read.
pub const ANNOTATIONS_MAX: usize = 100;
/// Most tags on one annotation.
pub const TAGS_MAX: usize = 4;

/// What each priority means unless the project's `[review.priorities]`
/// says otherwise.
pub fn default_meaning(priority: Priority) -> &'static str {
    match priority {
        Priority::P0 => "must be fixed before this is merged: it breaks something or is unsafe",
        Priority::P1 => "should be fixed before this is merged",
        Priority::P2 => "worth fixing, now or soon",
        Priority::P3 => "a matter of taste; take it or leave it",
    }
}

/// What a priority means in a project with `rules`.
pub fn meaning(priority: Priority, rules: &ukai_config::Review) -> String {
    rules
        .priorities
        .get(priority.name())
        .map(|meaning| meaning.trim().to_owned())
        .unwrap_or_else(|| default_meaning(priority).to_owned())
}

/// An annotation as the agent writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    pub path: String,
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub priority: Option<Priority>,
    pub tags: Vec<String>,
    pub body: String,
}

/// What keeps a review or a reply from being written; [`problems`] and
/// [`respond`] list them all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("the summary is over {} KiB", SUMMARY_MAX / 1024)]
    LongSummary,
    #[error("the summary has a character that does not show (U+{0:04X})")]
    InvisibleSummary(u32),
    #[error("a review has at most {ANNOTATIONS_MAX} annotations; keep the ones that matter")]
    TooMany,
    #[error("annotations[{0}]: `{1}` is not a path inside the checkout")]
    Path(usize, String),
    #[error("annotations[{0}]: lines start at 1, and `end_line` comes after `line`")]
    Range(usize),
    #[error("annotations[{0}]: say something")]
    EmptyBody(usize),
    #[error("annotations[{0}]: over {kib} KiB", kib = BODY_MAX / 1024)]
    LongBody(usize),
    #[error("annotations[{0}]: it has a character that does not show (U+{1:04X})")]
    Invisible(usize, u32),
    #[error("annotations[{0}]: at most {TAGS_MAX} tags")]
    TooManyTags(usize),
    #[error("annotations[{0}]: the tag `{1}` is not lowercase letters, digits, and `-`")]
    InvalidTag(usize, String),
    #[error("annotations[{0}]: the tag `{1}` is not one of this project's: {2}")]
    UnknownTag(usize, String, String),
    #[error("there is no annotation `{0}`")]
    UnknownAnnotation(String),
    #[error("reply, or resolve or reopen the annotation")]
    NothingToDo,
    #[error("the reply is empty")]
    EmptyReply,
    #[error("the reply is over {} KiB", BODY_MAX / 1024)]
    LongReply,
}

/// Everything that keeps `summary` and `drafts` from being written in a
/// project whose `[review]` is `rules`.
pub fn problems(
    summary: Option<&str>,
    drafts: &[Draft],
    rules: &ukai_config::Review,
) -> Vec<Problem> {
    let mut problems = Vec::new();
    if let Some(summary) = summary {
        if summary.len() > SUMMARY_MAX {
            problems.push(Problem::LongSummary);
        }
        if let Some(c) = invisible(summary) {
            problems.push(Problem::InvisibleSummary(u32::from(c)));
        }
    }
    if drafts.len() > ANNOTATIONS_MAX {
        problems.push(Problem::TooMany);
    }
    let known = rules.tags.keys().cloned().collect::<Vec<_>>().join(", ");
    for (index, draft) in drafts.iter().enumerate() {
        if !inside(&draft.path) {
            problems.push(Problem::Path(index, draft.path.clone()));
        }
        let range_ok = match (draft.line, draft.end_line) {
            (Some(0), _) | (None, Some(_)) => false,
            (Some(line), Some(end)) => end >= line,
            _ => true,
        };
        if !range_ok {
            problems.push(Problem::Range(index));
        }
        if draft.body.trim().is_empty() {
            problems.push(Problem::EmptyBody(index));
        }
        if draft.body.len() > BODY_MAX {
            problems.push(Problem::LongBody(index));
        }
        if let Some(c) = invisible(&draft.body) {
            problems.push(Problem::Invisible(index, u32::from(c)));
        }
        if draft.tags.len() > TAGS_MAX {
            problems.push(Problem::TooManyTags(index));
        }
        for tag in &draft.tags {
            if !is_tag(tag) {
                problems.push(Problem::InvalidTag(index, tag.clone()));
            } else if !rules.tags.is_empty() && !rules.tags.contains_key(tag) {
                problems.push(Problem::UnknownTag(index, tag.clone(), known.clone()));
            }
        }
    }
    problems
}

/// Whether `path` is relative and stays inside the checkout.
fn inside(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// The review once the agent writes `summary` (keeping the last one when
/// `None`) and `drafts`: they replace its annotations, except those with a
/// thread, which the user may still be in and which stay until resolved.
/// `anchor` reads the text of a line of the checkout's file as it is now.
pub fn write(
    current: Option<Review>,
    summary: Option<String>,
    drafts: Vec<Draft>,
    anchor: impl Fn(&str, u32) -> Option<String>,
    now: i64,
) -> Review {
    let current = current.unwrap_or_default();
    let first = next_number(&current.annotations);
    let mut annotations: Vec<Annotation> = current
        .annotations
        .into_iter()
        .filter(|annotation| !annotation.replies.is_empty())
        .collect();
    for (next, draft) in (first..).zip(drafts) {
        let shown_at = draft.end_line.or(draft.line);
        annotations.push(Annotation {
            id: format!("a{next}"),
            anchor: shown_at
                .and_then(|line| anchor(&draft.path, line))
                .unwrap_or_default(),
            path: draft.path,
            line: draft.line,
            end_line: draft.end_line,
            priority: draft.priority,
            tags: draft.tags,
            body: draft.body.trim().to_owned(),
            resolved: false,
            replies: Vec::new(),
        });
    }
    Review {
        summary: summary.map_or(current.summary, |summary| summary.trim().to_owned()),
        annotations,
        updated: now,
    }
}

/// The number after the highest of the `a<n>` ids, so an id is never
/// reused for another annotation.
fn next_number(annotations: &[Annotation]) -> u64 {
    annotations
        .iter()
        .filter_map(|annotation| annotation.id.strip_prefix('a')?.parse::<u64>().ok())
        .max()
        .map_or(1, |highest| highest + 1)
}

/// Reply to annotation `id` as `by`, and resolve or reopen it.
pub fn respond(
    review: &mut Review,
    id: &str,
    by: Author,
    reply: Option<&str>,
    resolved: Option<bool>,
    now: i64,
) -> Result<(), Vec<Problem>> {
    let reply = reply.map(str::trim);
    let mut problems = Vec::new();
    if reply.is_none() && resolved.is_none() {
        problems.push(Problem::NothingToDo);
    }
    if reply == Some("") {
        problems.push(Problem::EmptyReply);
    }
    if reply.is_some_and(|reply| reply.len() > BODY_MAX) {
        problems.push(Problem::LongReply);
    }
    let Some(annotation) = review.annotations.iter_mut().find(|a| a.id == id) else {
        problems.push(Problem::UnknownAnnotation(id.to_owned()));
        return Err(problems);
    };
    if !problems.is_empty() {
        return Err(problems);
    }
    if let Some(reply) = reply {
        annotation.replies.push(Reply {
            by,
            body: reply.to_owned(),
            at: now,
        });
    }
    if let Some(resolved) = resolved {
        annotation.resolved = resolved;
    }
    review.updated = now;
    Ok(())
}

/// Where an annotation shows on its file's diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// It is about the file as a whole.
    File,
    /// Under line `index` of hunk `hunk`; `outdated` when that line no
    /// longer reads as it did when the annotation was written.
    Line {
        hunk: usize,
        index: usize,
        outdated: bool,
    },
    /// Its line is not in the diff (any more), so it shows above it.
    Outside,
}

/// Where `annotation` shows on `file`'s diff.
pub fn place(annotation: &Annotation, file: &FileChange) -> Placement {
    let Some(at) = annotation.shown_at() else {
        return Placement::File;
    };
    let at = at as usize;
    for (hunk, lines) in file.hunks.iter().enumerate() {
        for (index, line) in lines.iter().enumerate() {
            if line.kind != LineKind::Removed && line.new == Some(at) {
                let outdated = !annotation.anchor.is_empty() && line.text != annotation.anchor;
                return Placement::Line {
                    hunk,
                    index,
                    outdated,
                };
            }
        }
    }
    Placement::Outside
}

/// Which annotations the diff pane shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Only these priorities; all when empty.
    pub priorities: Vec<Priority>,
    /// Only those with this tag.
    pub tag: Option<String>,
    /// Resolved ones too.
    pub resolved: bool,
}

impl Filter {
    pub fn shows(&self, annotation: &Annotation) -> bool {
        (self.resolved || !annotation.resolved)
            && (self.priorities.is_empty()
                || annotation
                    .priority
                    .is_some_and(|priority| self.priorities.contains(&priority)))
            && self
                .tag
                .as_ref()
                .is_none_or(|tag| annotation.tags.contains(tag))
    }

    /// Show `priority` too, or no longer, as a toggle of a chip.
    pub fn toggle(&mut self, priority: Priority) {
        match self.priorities.iter().position(|p| *p == priority) {
            Some(index) => {
                self.priorities.remove(index);
            }
            None => {
                self.priorities.push(priority);
                self.priorities.sort();
            }
        }
    }
}

/// The open annotations of `review` per priority, most pressing first, and
/// those with none.
pub fn open_counts(review: &Review) -> (Vec<(Priority, usize)>, usize) {
    let open = || review.annotations.iter().filter(|a| !a.resolved);
    let counts = Priority::ALL
        .into_iter()
        .map(|priority| {
            (
                priority,
                open().filter(|a| a.priority == Some(priority)).count(),
            )
        })
        .filter(|(_, count)| *count > 0)
        .collect();
    (counts, open().filter(|a| a.priority.is_none()).count())
}

/// The tags `review` uses, in order.
pub fn tags(review: &Review) -> Vec<String> {
    let mut tags: Vec<String> = review
        .annotations
        .iter()
        .flat_map(|annotation| annotation.tags.iter().cloned())
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

/// The most pressing priority of the open annotations on `path`, and how
/// many there are.
pub fn file_mark(review: &Review, path: &str) -> Option<(Option<Priority>, usize)> {
    let open: Vec<&Annotation> = review
        .annotations
        .iter()
        .filter(|a| a.path == path && !a.resolved)
        .collect();
    (!open.is_empty()).then(|| (open.iter().filter_map(|a| a.priority).min(), open.len()))
}

/// The user's `reply` to `annotation`, as a comment for the agent's next
/// prompt, naming the annotation so the agent can answer it in its thread.
pub fn reply_comment(annotation: &Annotation, reply: &str) -> ReviewComment {
    ReviewComment {
        path: annotation.path.clone(),
        line: DiffLine {
            kind: LineKind::Context,
            old: None,
            new: annotation.shown_at().map(|line| line as usize),
            text: annotation.anchor.clone(),
        },
        body: format!(
            "(on your annotation `{}`: {}) {}",
            annotation.id,
            first_line(&annotation.body),
            reply.trim()
        ),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut short: String = line.chars().take(80).collect();
    if short.len() < line.len() {
        short.push('…');
    }
    short
}

/// What the agent is told of the priorities and tags, from the project's
/// `[review]`.
pub fn rules_text(rules: &ukai_config::Review) -> String {
    let mut text = String::from("Priorities, most pressing first:\n");
    for priority in Priority::ALL {
        text.push_str(&format!(
            "- `{}`: {}\n",
            priority.name(),
            meaning(priority, rules)
        ));
    }
    if rules.tags.is_empty() {
        text.push_str(
            "Tags: any short words (lowercase letters, digits, `-`) that help the user sort \
             the annotations, such as `risk`, `question`, `test`, `perf`.\n",
        );
    } else {
        text.push_str("Tags: only these, which this project declares:\n");
        for (tag, meaning) in &rules.tags {
            text.push_str(&format!("- `{tag}`: {}\n", meaning.trim()));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rstest::rstest;

    use super::*;
    use crate::diff::ChangeKind;

    fn draft(path: &str, line: Option<u32>, body: &str) -> Draft {
        Draft {
            path: path.into(),
            line,
            body: body.into(),
            ..Draft::default()
        }
    }

    fn rules(tags: &[&str]) -> ukai_config::Review {
        ukai_config::Review {
            tags: tags
                .iter()
                .map(|tag| ((*tag).to_owned(), format!("{tag} things")))
                .collect(),
            priorities: BTreeMap::new(),
        }
    }

    #[rstest]
    #[case::fine(draft("src/lib.rs", Some(3), "ok"), vec![])]
    #[case::whole_file(draft("src/lib.rs", None, "ok"), vec![])]
    #[case::absolute(draft("/etc/passwd", Some(1), "x"), vec![Problem::Path(0, "/etc/passwd".into())])]
    #[case::parent(draft("../x", Some(1), "x"), vec![Problem::Path(0, "../x".into())])]
    #[case::empty_path(draft("", Some(1), "x"), vec![Problem::Path(0, String::new())])]
    #[case::line_zero(draft("a", Some(0), "x"), vec![Problem::Range(0)])]
    #[case::empty_body(draft("a", Some(1), "  "), vec![Problem::EmptyBody(0)])]
    #[case::long_body(draft("a", Some(1), &"x".repeat(BODY_MAX + 1)), vec![Problem::LongBody(0)])]
    #[case::invisible(draft("a", Some(1), "x\u{202e}"), vec![Problem::Invisible(0, 0x202E)])]
    fn checks_an_annotation(#[case] draft: Draft, #[case] expected: Vec<Problem>) {
        assert_eq!(problems(None, &[draft], &rules(&[])), expected);
    }

    #[test]
    fn checks_ranges_and_tags() {
        let backwards = Draft {
            end_line: Some(2),
            ..draft("a", Some(5), "x")
        };
        let end_only = Draft {
            end_line: Some(2),
            ..draft("a", None, "x")
        };
        let tagged = Draft {
            tags: vec!["Risk".into(), "perf".into(), "risk".into()],
            ..draft("a", Some(1), "x")
        };
        let many = Draft {
            tags: vec!["risk".into(); TAGS_MAX + 1],
            ..draft("a", Some(1), "x")
        };
        assert_eq!(
            problems(
                None,
                &[backwards, end_only, tagged, many],
                &rules(&["risk"])
            ),
            [
                Problem::Range(0),
                Problem::Range(1),
                Problem::InvalidTag(2, "Risk".into()),
                Problem::UnknownTag(2, "perf".into(), "risk".into()),
                Problem::TooManyTags(3),
            ]
        );
        // With no tags declared, any well-formed tag goes.
        let free = Draft {
            tags: vec!["perf".into()],
            ..draft("a", Some(1), "x")
        };
        assert!(problems(None, &[free], &rules(&[])).is_empty());
    }

    #[test]
    fn checks_the_summary_and_the_count() {
        let drafts = vec![draft("a", Some(1), "x"); ANNOTATIONS_MAX + 1];
        let summary = "y".repeat(SUMMARY_MAX + 1);
        assert_eq!(
            problems(Some(&summary), &drafts, &rules(&[])),
            [Problem::LongSummary, Problem::TooMany]
        );
        assert_eq!(
            problems(Some("hi\u{200b}"), &[], &rules(&[])),
            [Problem::InvisibleSummary(0x200B)]
        );
    }

    fn anchor(path: &str, line: u32) -> Option<String> {
        Some(format!("{path}:{line}"))
    }

    #[test]
    fn a_new_review_keeps_only_the_threads_of_the_last() {
        let first = write(
            None,
            Some("First pass.".into()),
            vec![
                draft("a.rs", Some(1), "one"),
                Draft {
                    end_line: Some(4),
                    ..draft("b.rs", Some(2), "two")
                },
                draft("c.rs", None, "three"),
            ],
            anchor,
            10,
        );
        let ids: Vec<&str> = first.annotations.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["a1", "a2", "a3"]);
        // Shown at the end of its range, and anchored there.
        assert_eq!(first.annotations[1].anchor, "b.rs:4");
        assert_eq!(first.annotations[2].anchor, "");

        let mut threaded = first.clone();
        assert_eq!(
            respond(&mut threaded, "a2", Author::User, Some("Why?"), None, 11),
            Ok(())
        );
        let second = write(
            Some(threaded),
            None,
            vec![draft("d.rs", Some(9), "four")],
            anchor,
            12,
        );
        let ids: Vec<&str> = second.annotations.iter().map(|a| a.id.as_str()).collect();
        // The thread stays, the rest go, and ids go on rather than repeat.
        assert_eq!(ids, ["a2", "a4"]);
        assert_eq!(second.summary, "First pass.");
        assert_eq!(second.updated, 12);
    }

    #[test]
    fn replies_and_resolves() {
        let mut review = write(None, None, vec![draft("a.rs", Some(1), "one")], anchor, 1);
        assert_eq!(
            respond(
                &mut review,
                "a1",
                Author::User,
                Some(" Is this safe? "),
                None,
                2
            ),
            Ok(())
        );
        assert_eq!(
            respond(
                &mut review,
                "a1",
                Author::Agent,
                Some("Yes."),
                Some(true),
                3
            ),
            Ok(())
        );
        let annotation = &review.annotations[0];
        assert!(annotation.resolved);
        assert_eq!(
            annotation.replies,
            [
                Reply {
                    by: Author::User,
                    body: "Is this safe?".into(),
                    at: 2
                },
                Reply {
                    by: Author::Agent,
                    body: "Yes.".into(),
                    at: 3
                },
            ]
        );
        assert_eq!(review.updated, 3);
    }

    #[rstest]
    #[case::nothing("a1", None, None, vec![Problem::NothingToDo])]
    #[case::empty("a1", Some("  ".into()), None, vec![Problem::EmptyReply])]
    #[case::long("a1", Some("x".repeat(BODY_MAX + 1)), None, vec![Problem::LongReply])]
    #[case::unknown("a9", Some("hi".into()), None, vec![Problem::UnknownAnnotation("a9".into())])]
    fn refuses_a_reply(
        #[case] id: &str,
        #[case] reply: Option<String>,
        #[case] resolved: Option<bool>,
        #[case] expected: Vec<Problem>,
    ) {
        let mut review = write(None, None, vec![draft("a.rs", Some(1), "one")], anchor, 1);
        let before = review.clone();
        assert_eq!(
            respond(&mut review, id, Author::User, reply.as_deref(), resolved, 2),
            Err(expected)
        );
        assert_eq!(review, before);
    }

    fn file() -> FileChange {
        let line = |kind, old, new, text: &str| DiffLine {
            kind,
            old,
            new,
            text: text.into(),
        };
        FileChange {
            path: "a.rs".into(),
            kind: ChangeKind::Modified,
            binary: false,
            hunks: vec![
                vec![
                    line(LineKind::Context, Some(1), Some(1), "fn a() {}"),
                    line(LineKind::Removed, Some(2), None, "fn b() {}"),
                    line(LineKind::Added, None, Some(2), "fn b() -> u8 { 1 }"),
                ],
                vec![line(LineKind::Added, None, Some(20), "fn z() {}")],
            ],
        }
    }

    fn at(line: Option<u32>, anchor: &str) -> Annotation {
        Annotation {
            id: "a1".into(),
            path: "a.rs".into(),
            line,
            anchor: anchor.into(),
            body: "x".into(),
            ..Annotation::default()
        }
    }

    #[rstest]
    #[case::whole_file(at(None, ""), Placement::File)]
    #[case::on_its_line(at(Some(2), "fn b() -> u8 { 1 }"), Placement::Line { hunk: 0, index: 2, outdated: false })]
    #[case::changed_under_it(at(Some(2), "fn b() {}"), Placement::Line { hunk: 0, index: 2, outdated: true })]
    #[case::second_hunk(at(Some(20), "fn z() {}"), Placement::Line { hunk: 1, index: 0, outdated: false })]
    #[case::not_in_the_diff(at(Some(9), "x"), Placement::Outside)]
    fn places_an_annotation(#[case] annotation: Annotation, #[case] expected: Placement) {
        assert_eq!(place(&annotation, &file()), expected);
    }

    #[test]
    fn filters_by_priority_tag_and_resolution() {
        let annotation = |priority, tags: &[&str], resolved| Annotation {
            priority,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            resolved,
            ..Annotation::default()
        };
        let p1_risk = annotation(Some(Priority::P1), &["risk"], false);
        let p3 = annotation(Some(Priority::P3), &[], false);
        let none = annotation(None, &["risk"], false);
        let done = annotation(Some(Priority::P1), &[], true);

        let mut filter = Filter::default();
        assert!(filter.shows(&p1_risk) && filter.shows(&p3) && filter.shows(&none));
        assert!(!filter.shows(&done));
        filter.toggle(Priority::P1);
        assert!(filter.shows(&p1_risk) && !filter.shows(&p3) && !filter.shows(&none));
        filter.toggle(Priority::P1);
        filter.tag = Some("risk".into());
        assert!(filter.shows(&p1_risk) && filter.shows(&none) && !filter.shows(&p3));
        filter.tag = None;
        filter.resolved = true;
        assert!(filter.shows(&done));
    }

    #[test]
    fn counts_and_marks() {
        let review = Review {
            annotations: vec![
                Annotation {
                    path: "a.rs".into(),
                    priority: Some(Priority::P2),
                    tags: vec!["perf".into()],
                    ..Annotation::default()
                },
                Annotation {
                    path: "a.rs".into(),
                    priority: Some(Priority::P0),
                    tags: vec!["risk".into(), "perf".into()],
                    ..Annotation::default()
                },
                Annotation {
                    path: "b.rs".into(),
                    ..Annotation::default()
                },
                Annotation {
                    path: "b.rs".into(),
                    priority: Some(Priority::P0),
                    resolved: true,
                    ..Annotation::default()
                },
            ],
            ..Review::default()
        };
        assert_eq!(
            open_counts(&review),
            (vec![(Priority::P0, 1), (Priority::P2, 1)], 1)
        );
        assert_eq!(tags(&review), ["perf", "risk"]);
        assert_eq!(file_mark(&review, "a.rs"), Some((Some(Priority::P0), 2)));
        assert_eq!(file_mark(&review, "b.rs"), Some((None, 1)));
        assert_eq!(file_mark(&review, "c.rs"), None);
    }

    #[test]
    fn a_reply_names_the_annotation_for_the_agent() {
        let annotation = Annotation {
            id: "a3".into(),
            path: "src/lib.rs".into(),
            line: Some(10),
            end_line: Some(12),
            anchor: "    x.unwrap()".into(),
            body: "This unwraps a user's input.\nMore here.".into(),
            ..Annotation::default()
        };
        let comment = reply_comment(&annotation, " Use `?` instead. ");
        assert_eq!(comment.location(), "src/lib.rs:12");
        assert_eq!(
            comment.body,
            "(on your annotation `a3`: This unwraps a user's input.) Use `?` instead."
        );
    }

    #[test]
    fn tells_the_agent_the_rules() {
        let mut project = rules(&["risk"]);
        project.priorities.insert("p0".into(), "breaks prod".into());
        let text = rules_text(&project);
        assert!(text.contains("- `p0`: breaks prod"), "{text}");
        assert!(
            text.contains(&format!("- `p3`: {}", default_meaning(Priority::P3))),
            "{text}"
        );
        assert!(
            text.contains("only these") && text.contains("- `risk`: risk things"),
            "{text}"
        );
        assert!(rules_text(&rules(&[])).contains("any short words"));
    }
}
