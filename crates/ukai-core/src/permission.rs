//! Answering an agent's permission requests without asking: by the rules of
//! the configuration (`[[permissions]]`), then by what the user chose to
//! always allow from a request's card ([`Saved`], kept with the project).
//!
//! A request is answered by the first rule that matches it. A rule that
//! decides `ask` stops the search, so the configuration can keep a request
//! in front of the user whatever was saved from a card.

use serde::{Deserialize, Serialize};
use ukai_acp::PermissionChoice;
use ukai_config::{Decision, PermissionRule, title_glob};

/// What an agent asks to do, as a rule sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Asked<'a> {
    /// The id the pane opened the agent by.
    pub agent: &'a str,
    /// The tool call's ACP `ToolKind`, when known.
    pub kind: Option<&'a str>,
    /// The request's title, as its card shows it.
    pub title: &'a str,
}

/// Something the user chose to always allow from a request's card. Unlike a
/// configured rule, its title is the whole title and not a glob: it allows
/// what the user saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub agent: String,
    /// Every kind when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_kind: Option<String>,
    /// Every title when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl Saved {
    /// Always allow exactly what `asked` asks.
    pub fn this(asked: Asked) -> Self {
        Self {
            agent: asked.agent.to_owned(),
            tool_kind: asked.kind.map(str::to_owned),
            title: Some(asked.title.to_owned()),
        }
    }

    /// Always allow every request of `asked`'s kind, when it has one: a
    /// rule for every request of an agent is written in the configuration,
    /// not made by a click.
    pub fn kind(asked: Asked) -> Option<Self> {
        Some(Self {
            agent: asked.agent.to_owned(),
            tool_kind: Some(asked.kind?.to_owned()),
            title: None,
        })
    }

    fn matches(&self, asked: Asked) -> bool {
        self.agent == asked.agent
            && self
                .tool_kind
                .as_deref()
                .is_none_or(|kind| asked.kind == Some(kind))
            && self
                .title
                .as_deref()
                .is_none_or(|title| title == asked.title)
    }

    /// What the rule allows, for a card and a button: `every read` or
    /// `` `cargo test` ``.
    pub fn label(&self) -> String {
        match (&self.tool_kind, &self.title) {
            (_, Some(title)) => format!("`{title}`"),
            (Some(kind), None) => format!("every {kind}"),
            (None, None) => "everything".to_owned(),
        }
    }
}

/// Why a request was answered without asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A `[[permissions]]` rule of the configuration.
    Config,
    /// What the user saved from a card, which they can forget again.
    Saved(Saved),
}

/// An answer given without asking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// Which of the request's options to choose.
    pub option: PermissionChoice,
    pub origin: Origin,
}

impl Answer {
    /// What the request's card says was answered.
    pub fn label(&self) -> String {
        match &self.origin {
            Origin::Config => format!("{} (by a configured rule)", self.option.name),
            Origin::Saved(_) => format!("{} (always allowed)", self.option.name),
        }
    }
}

/// The answer to `asked` that needs no asking, if `rules` or `saved` give
/// one among `options`. A rule that allows or denies a request whose
/// options offer no such answer leaves it to the user.
pub fn decide(
    rules: &[PermissionRule],
    saved: &[Saved],
    asked: Asked,
    options: &[PermissionChoice],
) -> Option<Answer> {
    if let Some(rule) = rules.iter().find(|rule| rule_matches(rule, asked)) {
        let kinds = match rule.decision {
            Decision::Allow => ["allow_once", "allow_always"],
            Decision::Deny => ["reject_once", "reject_always"],
            Decision::Ask => return None,
        };
        return choose(options, kinds).map(|option| Answer {
            option,
            origin: Origin::Config,
        });
    }
    let saved = saved.iter().find(|saved| saved.matches(asked))?;
    choose(options, ["allow_once", "allow_always"]).map(|option| Answer {
        option,
        origin: Origin::Saved(saved.clone()),
    })
}

/// The option that allows the request this once, for a card's "always"
/// buttons to answer with: what the agent itself remembers of an
/// `allow_always` is not what the user asked ukai to remember.
pub fn allowing(options: &[PermissionChoice]) -> Option<PermissionChoice> {
    choose(options, ["allow_once", "allow_always"])
}

/// The first option of the first of `kinds` that any option has.
fn choose(options: &[PermissionChoice], kinds: [&str; 2]) -> Option<PermissionChoice> {
    kinds
        .iter()
        .find_map(|kind| options.iter().find(|option| option.kind == *kind))
        .cloned()
}

fn rule_matches(rule: &PermissionRule, asked: Asked) -> bool {
    rule.agent
        .as_deref()
        .is_none_or(|agent| agent == asked.agent)
        && rule
            .tool_kind
            .as_deref()
            .is_none_or(|kind| asked.kind == Some(kind))
        // A glob that does not parse was reported when the configuration
        // loaded; here it matches nothing.
        && rule.title.as_deref().is_none_or(|title| {
            title_glob(title).is_ok_and(|glob| glob.is_match(asked.title))
        })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn option(id: &str, kind: &str) -> PermissionChoice {
        PermissionChoice {
            id: id.to_owned(),
            name: id.to_owned(),
            kind: kind.to_owned(),
        }
    }

    fn options() -> Vec<PermissionChoice> {
        vec![
            option("always", "allow_always"),
            option("allow", "allow_once"),
            option("reject", "reject_once"),
        ]
    }

    fn rule(
        agent: Option<&str>,
        kind: Option<&str>,
        title: Option<&str>,
        decision: Decision,
    ) -> PermissionRule {
        PermissionRule {
            agent: agent.map(str::to_owned),
            tool_kind: kind.map(str::to_owned),
            title: title.map(str::to_owned),
            decision,
        }
    }

    /// What `Saved::kind(ASKED)` saves.
    fn every_execute() -> Saved {
        Saved {
            agent: "claude".to_owned(),
            tool_kind: Some("execute".to_owned()),
            title: None,
        }
    }

    #[test]
    fn saving_a_kind_saves_every_request_of_it() {
        assert_eq!(Saved::kind(ASKED), Some(every_execute()));
    }

    const ASKED: Asked = Asked {
        agent: "claude",
        kind: Some("execute"),
        title: "cargo test -p ukai-core",
    };

    #[rstest]
    #[case::every_request(rule(None, None, None, Decision::Allow), Some("allow"))]
    #[case::denied(rule(None, None, None, Decision::Deny), Some("reject"))]
    #[case::asked(rule(None, None, None, Decision::Ask), None)]
    #[case::its_agent(rule(Some("claude"), None, None, Decision::Allow), Some("allow"))]
    #[case::another_agent(rule(Some("codex"), None, None, Decision::Allow), None)]
    #[case::its_kind(rule(None, Some("execute"), None, Decision::Allow), Some("allow"))]
    #[case::another_kind(rule(None, Some("read"), None, Decision::Allow), None)]
    #[case::its_title(rule(None, None, Some("cargo test*"), Decision::Allow), Some("allow"))]
    #[case::the_whole_title(rule(None, None, Some("cargo"), Decision::Allow), None)]
    #[case::another_title(rule(None, None, Some("rm *"), Decision::Allow), None)]
    #[case::all_three(
        rule(Some("claude"), Some("execute"), Some("cargo *"), Decision::Deny),
        Some("reject")
    )]
    #[case::an_unreadable_title(rule(None, None, Some("["), Decision::Allow), None)]
    fn a_rule_answers_what_it_matches(#[case] rule: PermissionRule, #[case] chosen: Option<&str>) {
        let answer = decide(&[rule], &[], ASKED, &options());
        assert_eq!(
            answer.as_ref().map(|answer| answer.option.id.as_str()),
            chosen
        );
        assert!(answer.is_none_or(|answer| answer.origin == Origin::Config));
    }

    #[test]
    fn a_request_without_a_kind_matches_no_rule_of_a_kind() {
        let asked = Asked {
            kind: None,
            ..ASKED
        };
        let rules = [rule(None, Some("execute"), None, Decision::Allow)];
        assert_eq!(decide(&rules, &[], asked, &options()), None);
        let kind = every_execute();
        assert_eq!(decide(&[], &[kind], asked, &options()), None);
        assert_eq!(Saved::kind(asked), None);
    }

    #[test]
    fn the_first_matching_rule_answers() {
        let rules = [
            rule(None, None, Some("cargo publish*"), Decision::Deny),
            rule(None, None, Some("cargo test*"), Decision::Ask),
            rule(None, Some("execute"), None, Decision::Allow),
        ];
        let answer = |title| {
            decide(&rules, &[], Asked { title, ..ASKED }, &options()).map(|answer| answer.option.id)
        };
        assert_eq!(answer("cargo publish").as_deref(), Some("reject"));
        assert_eq!(answer("cargo test"), None);
        assert_eq!(answer("ls").as_deref(), Some("allow"));
    }

    #[rstest]
    #[case::this(Saved::this(ASKED), "cargo test -p ukai-core", Some("execute"), true)]
    #[case::this_not_as_a_glob(Saved::this(Asked { title: "cargo *", ..ASKED }), "cargo test", Some("execute"), false)]
    #[case::this_of_another_kind(
        Saved::this(ASKED),
        "cargo test -p ukai-core",
        Some("read"),
        false
    )]
    #[case::kind(every_execute(), "rm -rf /", Some("execute"), true)]
    #[case::another_kind(every_execute(), "rm -rf /", Some("edit"), false)]
    fn what_was_saved_allows_what_it_matches(
        #[case] saved: Saved,
        #[case] title: &str,
        #[case] kind: Option<&str>,
        #[case] allowed: bool,
    ) {
        let asked = Asked {
            title,
            kind,
            ..ASKED
        };
        let answer = decide(&[], std::slice::from_ref(&saved), asked, &options());
        assert_eq!(
            answer,
            allowed.then(|| Answer {
                option: option("allow", "allow_once"),
                origin: Origin::Saved(saved),
            })
        );
    }

    #[test]
    fn what_was_saved_is_another_agents_to_ask() {
        let saved = [Saved::this(ASKED)];
        let asked = Asked {
            agent: "codex",
            ..ASKED
        };
        assert_eq!(decide(&[], &saved, asked, &options()), None);
    }

    #[rstest]
    #[case::asked(Decision::Ask, None)]
    #[case::denied(Decision::Deny, Some("reject"))]
    fn a_configured_rule_comes_before_what_was_saved(
        #[case] decision: Decision,
        #[case] chosen: Option<&str>,
    ) {
        let rules = [rule(None, Some("execute"), None, decision)];
        let answer = decide(&rules, &[Saved::this(ASKED)], ASKED, &options());
        assert_eq!(answer.map(|answer| answer.option.id).as_deref(), chosen);
    }

    #[rstest]
    #[case::allow_without_an_allowing_option(Decision::Allow, vec![option("reject", "reject_once")], None)]
    #[case::deny_without_a_rejecting_option(Decision::Deny, vec![option("allow", "allow_once")], None)]
    #[case::allow_always_when_there_is_no_once(Decision::Allow, vec![option("always", "allow_always")], Some("always"))]
    #[case::reject_always_when_there_is_no_once(Decision::Deny, vec![option("never", "reject_always")], Some("never"))]
    fn a_rule_answers_only_with_an_option_the_agent_offers(
        #[case] decision: Decision,
        #[case] options: Vec<PermissionChoice>,
        #[case] chosen: Option<&str>,
    ) {
        let rules = [rule(None, None, None, decision)];
        let answer = decide(&rules, &[], ASKED, &options);
        assert_eq!(answer.map(|answer| answer.option.id).as_deref(), chosen);
    }

    #[rstest]
    #[case::this(Saved::this(ASKED), "`cargo test -p ukai-core`")]
    #[case::kind(every_execute(), "every execute")]
    fn says_what_was_saved(#[case] saved: Saved, #[case] label: &str) {
        assert_eq!(saved.label(), label);
    }

    #[test]
    fn says_why_a_request_was_answered() {
        let by = |origin| {
            Answer {
                option: option("Allow", "allow_once"),
                origin,
            }
            .label()
        };
        assert_eq!(by(Origin::Config), "Allow (by a configured rule)");
        assert_eq!(
            by(Origin::Saved(Saved::this(ASKED))),
            "Allow (always allowed)"
        );
    }
}
