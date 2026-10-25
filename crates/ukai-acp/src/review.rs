//! An agent's review of its checkout's changes, as the diff pane shows it:
//! a summary, and annotations on lines of the diff with a priority and tags,
//! each with a thread of replies between the user and the agent.
//!
//! A gateway agent writes it through the relay's MCP server (`review_diff`),
//! the relay keeps it next to the checkout and sends it with the diff
//! ([`crate::checkout::CheckoutDiff`]), and the user's replies and resolves
//! come back through [`REVIEW_PATH`] (`ukai.gateway.v1.Files.Annotate`).
//! What may be written is `ukai_core::review`'s to say.

use serde::{Deserialize, Serialize};
use ukai_proto::gateway::v1 as gw;

/// Where a relay takes the user's reply to an annotation, or its resolving:
/// an [`AnnotateRequest`] posted as JSON, answered with the [`Review`]; 422
/// with what is wrong when it is refused.
pub const REVIEW_PATH: &str = "/review";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// What the agent says of the changes as a whole, in Markdown.
    pub summary: String,
    pub annotations: Vec<Annotation>,
    /// When it last changed, in seconds since the epoch.
    pub updated: i64,
}

/// How pressing an annotation is, `P0` most.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

impl Priority {
    pub const ALL: [Self; 4] = [Self::P0, Self::P1, Self::P2, Self::P3];

    pub fn name(self) -> &'static str {
        match self {
            Self::P0 => "p0",
            Self::P1 => "p1",
            Self::P2 => "p2",
            Self::P3 => "p3",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// A remark on a line, or a range of lines, of the diff's new side, or on a
/// file as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    /// `a1`, `a2`, … in the order they were written.
    pub id: String,
    /// Relative to the checkout's root, `/`-separated.
    pub path: String,
    /// 1-based, on the new side; `None` for the file as a whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// The last line of a range starting at `line`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Markdown.
    pub body: String,
    /// The text of the line it is shown at (`end_line`, else `line`) when it
    /// was written, so a change under it shows.
    #[serde(default)]
    pub anchor: String,
    #[serde(default)]
    pub resolved: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replies: Vec<Reply>,
}

impl Annotation {
    /// The line it is shown at: the end of its range.
    pub fn shown_at(&self) -> Option<u32> {
        self.end_line.or(self.line)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Author {
    Agent,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub by: Author,
    /// Markdown.
    pub body: String,
    /// Seconds since the epoch.
    pub at: i64,
}

/// What the user does to an annotation: reply, resolve or reopen it, or
/// both at once.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotateRequest {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved: Option<bool>,
}

fn wire_priority(priority: Option<Priority>) -> gw::Priority {
    match priority {
        None => gw::Priority::None,
        Some(Priority::P0) => gw::Priority::P0,
        Some(Priority::P1) => gw::Priority::P1,
        Some(Priority::P2) => gw::Priority::P2,
        Some(Priority::P3) => gw::Priority::P3,
    }
}

fn priority_of(priority: gw::Priority) -> Option<Priority> {
    match priority {
        gw::Priority::None => None,
        gw::Priority::P0 => Some(Priority::P0),
        gw::Priority::P1 => Some(Priority::P1),
        gw::Priority::P2 => Some(Priority::P2),
        gw::Priority::P3 => Some(Priority::P3),
    }
}

impl From<Review> for gw::Review {
    fn from(review: Review) -> Self {
        Self {
            summary: review.summary,
            annotations: review.annotations.into_iter().map(Into::into).collect(),
            updated: review.updated,
        }
    }
}

impl From<gw::Review> for Review {
    fn from(review: gw::Review) -> Self {
        Self {
            summary: review.summary,
            annotations: review.annotations.into_iter().map(Into::into).collect(),
            updated: review.updated,
        }
    }
}

impl From<Annotation> for gw::Annotation {
    fn from(annotation: Annotation) -> Self {
        Self {
            id: annotation.id,
            path: annotation.path,
            line: annotation.line,
            end_line: annotation.end_line,
            priority: wire_priority(annotation.priority).into(),
            tags: annotation.tags,
            body: annotation.body,
            anchor: annotation.anchor,
            resolved: annotation.resolved,
            replies: annotation.replies.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<gw::Annotation> for Annotation {
    fn from(annotation: gw::Annotation) -> Self {
        Self {
            priority: priority_of(annotation.priority()),
            id: annotation.id,
            path: annotation.path,
            line: annotation.line,
            end_line: annotation.end_line,
            tags: annotation.tags,
            body: annotation.body,
            anchor: annotation.anchor,
            resolved: annotation.resolved,
            replies: annotation.replies.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<Reply> for gw::Reply {
    fn from(reply: Reply) -> Self {
        let by = match reply.by {
            Author::Agent => gw::Author::Agent,
            Author::User => gw::Author::User,
        };
        Self {
            by: by.into(),
            body: reply.body,
            at: reply.at,
        }
    }
}

impl From<gw::Reply> for Reply {
    fn from(reply: gw::Reply) -> Self {
        let by = match reply.by() {
            gw::Author::Agent => Author::Agent,
            gw::Author::User => Author::User,
        };
        Self {
            by,
            body: reply.body,
            at: reply.at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review() -> Review {
        Review {
            summary: "Moves the parser.".into(),
            annotations: vec![
                Annotation {
                    id: "a1".into(),
                    path: "src/lib.rs".into(),
                    line: Some(10),
                    end_line: Some(12),
                    priority: Some(Priority::P1),
                    tags: vec!["risk".into()],
                    body: "Unwraps here.".into(),
                    anchor: "    x.unwrap()".into(),
                    resolved: false,
                    replies: vec![
                        Reply {
                            by: Author::User,
                            body: "Why?".into(),
                            at: 5,
                        },
                        Reply {
                            by: Author::Agent,
                            body: "Fixed.".into(),
                            at: 6,
                        },
                    ],
                },
                Annotation {
                    id: "a2".into(),
                    path: "README.md".into(),
                    body: "Docs moved.".into(),
                    resolved: true,
                    ..Annotation::default()
                },
            ],
            updated: 7,
        }
    }

    #[test]
    fn a_review_crosses_the_wire_unchanged() {
        let review = review();
        assert_eq!(Review::from(gw::Review::from(review.clone())), review);
    }

    #[test]
    fn a_review_reads_back_from_json() {
        let review = review();
        let json = serde_json::to_string(&review).unwrap_or_default();
        assert!(json.contains(r#""priority":"p1""#), "{json}");
        let back: Result<Review, _> = serde_json::from_str(&json);
        assert_eq!(back.ok(), Some(review));
    }

    #[test]
    fn names_priorities() {
        for priority in Priority::ALL {
            assert_eq!(Priority::parse(priority.name()), Some(priority));
        }
        assert_eq!(Priority::parse("p4"), None);
        assert!(Priority::P0 < Priority::P3);
    }
}
