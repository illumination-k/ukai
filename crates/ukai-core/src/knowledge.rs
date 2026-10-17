//! Knowledge a gateway keeps for a repository outside it: what a valid note
//! is, and which repository a git URL names. Pure, so the gateway that
//! stores notes and the app that edits them agree, and so it is tested here.
//!
//! Every later session on the repository is shown a note's name and
//! description and may read its body, so a note is checked for what should
//! never reach them that way: characters a reviewer cannot see (the "rules
//! file backdoor") and credentials.
//!
//! The types are [`ukai_acp::knowledge`]'s, since they cross the wire.

use std::sync::LazyLock;

use regex::Regex;
pub use ukai_acp::knowledge::{
    Approved, ApprovedSummary, Conflict, Editor, Note, NoteSpec, NoteSummary, NoteWrite,
    Provenance, Revision,
};

/// Longest name: short enough for a ConfigMap name and a tool argument.
pub const NAME_MAX: usize = 64;
/// Longest description, in characters: one line in a listing.
pub const DESCRIPTION_MAX: usize = 200;
/// Largest body, in bytes. A note is one ConfigMap, which holds at most
/// 1 MiB; a note longer than this is several notes.
pub const BODY_MAX: usize = 256 * 1024;

/// What can be wrong with a note; [`problems`] lists them all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("give the note a name")]
    EmptyName,
    #[error("a name is lowercase letters, digits, and `-`, like `release-process`")]
    InvalidName,
    #[error("a name is at most {NAME_MAX} characters")]
    LongName,
    #[error("give the note a one-line description")]
    EmptyDescription,
    #[error("the description is one line")]
    MultilineDescription,
    #[error("the description is at most {DESCRIPTION_MAX} characters")]
    LongDescription,
    #[error("the note is empty")]
    EmptyBody,
    #[error("the note is over {} KiB; split it", BODY_MAX / 1024)]
    LongBody,
    #[error("the {0} has a character that does not show (U+{1:04X}); remove it")]
    Invisible(Field, u32),
    #[error("the {0} looks like it holds a credential ({1}); never put one in a note")]
    Secret(Field, &'static str),
}

/// The part of a note a problem is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Description,
    Body,
}

impl std::fmt::Display for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Field::Name => "name",
            Field::Description => "description",
            Field::Body => "note",
        })
    }
}

/// Everything that keeps `spec` from being saved as `name`.
pub fn problems(name: &str, spec: &NoteSpec) -> Vec<Problem> {
    let mut problems = name_problems(name);
    let description = spec.description.trim();
    if description.is_empty() {
        problems.push(Problem::EmptyDescription);
    }
    if description.contains(['\n', '\r']) {
        problems.push(Problem::MultilineDescription);
    }
    if description.chars().count() > DESCRIPTION_MAX {
        problems.push(Problem::LongDescription);
    }
    if spec.body.trim().is_empty() {
        problems.push(Problem::EmptyBody);
    }
    if spec.body.len() > BODY_MAX {
        problems.push(Problem::LongBody);
    }
    for (field, text) in [
        (Field::Description, spec.description.as_str()),
        (Field::Body, spec.body.as_str()),
    ] {
        if let Some(c) = invisible(text) {
            problems.push(Problem::Invisible(field, u32::from(c)));
        }
        if let Some(kind) = secret(text) {
            problems.push(Problem::Secret(field, kind));
        }
    }
    problems
}

/// The first character in `text` that renders as nothing or reorders what
/// is around it: zero-width and joining characters, bidirectional controls,
/// and Unicode tags, which can carry instructions a reviewer never sees.
fn invisible(text: &str) -> Option<char> {
    text.chars().find(|c| {
        matches!(
            u32::from(*c),
            0x00AD
                | 0x061C
                | 0x180E
                | 0x200B..=0x200F
                | 0x202A..=0x202E
                | 0x2060..=0x2064
                | 0x2066..=0x206F
                | 0xFEFF
                | 0xFFF9..=0xFFFB
                | 0xE0000..=0xE007F
        )
    })
}

/// Shapes of credentials common enough to look for, by what they are.
static SECRETS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        ("a private key", r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
        (
            "a GitHub token",
            r"\b(gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{60,})",
        ),
        ("an AWS access key", r"\b(AKIA|ASIA)[0-9A-Z]{16}\b"),
        ("a Google API key", r"\bAIza[0-9A-Za-z_-]{35}"),
        ("a Slack token", r"\bxox[abprs]-[0-9A-Za-z-]{10,}"),
        ("an Anthropic API key", r"\bsk-ant-[0-9A-Za-z_-]{20,}"),
        ("an OpenAI API key", r"\bsk-(proj-)?[0-9A-Za-z_-]{32,}"),
        (
            "a JSON web token",
            r"\beyJ[0-9A-Za-z_-]{10,}\.eyJ[0-9A-Za-z_-]{10,}\.[0-9A-Za-z_-]{10,}",
        ),
    ]
    .into_iter()
    .filter_map(|(kind, pattern)| Regex::new(pattern).ok().map(|regex| (kind, regex)))
    .collect()
});

/// What kind of credential `text` seems to hold, if any.
fn secret(text: &str) -> Option<&'static str> {
    SECRETS
        .iter()
        .find(|(_, regex)| regex.is_match(text))
        .map(|(kind, _)| *kind)
}

/// Everything wrong with `name` alone.
pub fn name_problems(name: &str) -> Vec<Problem> {
    let mut problems = Vec::new();
    if name.is_empty() {
        problems.push(Problem::EmptyName);
        return problems;
    }
    let valid = name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !valid {
        problems.push(Problem::InvalidName);
    }
    if name.len() > NAME_MAX {
        problems.push(Problem::LongName);
    }
    problems
}

/// The repository a git URL names, the same for every way of writing it:
/// `https://github.com/o/r.git`, `git@github.com:o/r`, and
/// `ssh://git@GitHub.com/o/r/` are all `github.com/o/r`. Notes are kept
/// under it, so agents cloning one repository by different URLs share them.
pub fn repository_key(git: &str) -> String {
    let url = git.trim();
    let (rest, scp) = match url.split_once("://") {
        Some((_, rest)) => (rest, false),
        None => (url, true),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    // Credentials or a user name before the host.
    let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
    let rest = match authority.rfind('@') {
        Some(at) => &rest[at + 1..],
        None => rest,
    };
    // scp-like `host:path`, which has no scheme.
    let (host, path) = match (scp, rest.split_once(':')) {
        (true, Some((host, path))) if !host.contains('/') => (host, path),
        _ => rest.split_once('/').unwrap_or((rest, "")),
    };
    // A port is not part of which repository it is.
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.is_empty() {
        host
    } else {
        format!("{host}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn spec(description: &str, body: &str) -> NoteSpec {
        NoteSpec {
            description: description.into(),
            body: body.into(),
        }
    }

    #[rstest]
    #[case::env_reference("Set `${env:UKAI_GATEWAY_TOKEN}`; never the value.")]
    #[case::short_sk("Use `sk-` prefixed keys from the keychain.")]
    #[case::emoji("Ship it 🚀, see `docs/design.md`.")]
    #[case::japanese("ビルドは `GGML_NATIVE=OFF` を付ける。")]
    fn ordinary_notes_are_not_mistaken_for_secrets_or_tricks(#[case] body: &str) {
        assert_eq!(problems("n", &spec("d", body)), []);
    }

    #[test]
    fn a_good_note_has_no_problems() {
        assert_eq!(
            problems("release-process", &spec("How we cut a release", "1. tag")),
            []
        );
    }

    #[rstest]
    #[case::empty("", vec![Problem::EmptyName])]
    #[case::upper("Release", vec![Problem::InvalidName])]
    #[case::space("a b", vec![Problem::InvalidName])]
    #[case::slash("a/b", vec![Problem::InvalidName])]
    #[case::dots("..", vec![Problem::InvalidName])]
    #[case::leading_dash("-a", vec![Problem::InvalidName])]
    #[case::trailing_dash("a-", vec![Problem::InvalidName])]
    #[case::long(&"a".repeat(NAME_MAX + 1), vec![Problem::LongName])]
    #[case::long_and_invalid(&"A".repeat(NAME_MAX + 1), vec![Problem::InvalidName, Problem::LongName])]
    #[case::longest(&"a".repeat(NAME_MAX), vec![])]
    fn names(#[case] name: &str, #[case] expected: Vec<Problem>) {
        assert_eq!(name_problems(name), expected);
    }

    #[rstest]
    #[case::empty_description(spec(" ", "x"), vec![Problem::EmptyDescription])]
    #[case::multiline(spec("a\nb", "x"), vec![Problem::MultilineDescription])]
    #[case::long_description(spec(&"é".repeat(DESCRIPTION_MAX + 1), "x"), vec![Problem::LongDescription])]
    #[case::longest_description(spec(&"é".repeat(DESCRIPTION_MAX), "x"), vec![])]
    #[case::empty_body(spec("d", "\n "), vec![Problem::EmptyBody])]
    #[case::long_body(spec("d", &"x".repeat(BODY_MAX + 1)), vec![Problem::LongBody])]
    #[case::everything(spec("", ""), vec![Problem::EmptyName, Problem::EmptyDescription, Problem::EmptyBody])]
    #[case::zero_width(spec("d\u{200B}", "x"), vec![Problem::Invisible(Field::Description, 0x200B)])]
    #[case::bidi(spec("d", "run \u{202E}lmth.exe"), vec![Problem::Invisible(Field::Body, 0x202E)])]
    #[case::tag(spec("d", "ok\u{E0041}"), vec![Problem::Invisible(Field::Body, 0xE0041)])]
    #[case::github_token(
        spec("d", &format!("token: ghp_{}", "a".repeat(36))),
        vec![Problem::Secret(Field::Body, "a GitHub token")]
    )]
    #[case::aws(spec("AKIAABCDEFGHIJKLMNOP", "x"), vec![Problem::Secret(Field::Description, "an AWS access key")])]
    #[case::private_key(
        // Put together here so the repository's own private key check passes.
        spec("d", &format!("-----BEGIN {} PRIVATE KEY-----\nabc", "OPENSSH")),
        vec![Problem::Secret(Field::Body, "a private key")]
    )]
    #[case::anthropic(
        spec("d", &format!("sk-ant-api03-{}", "x".repeat(40))),
        vec![Problem::Secret(Field::Body, "an Anthropic API key")]
    )]
    #[case::both_fields(
        spec("d\u{2066}", &format!("xoxb-{}", "1".repeat(20))),
        vec![Problem::Invisible(Field::Description, 0x2066), Problem::Secret(Field::Body, "a Slack token")]
    )]
    fn collects_every_problem(#[case] spec: NoteSpec, #[case] expected: Vec<Problem>) {
        let name = if expected.contains(&Problem::EmptyName) {
            ""
        } else {
            "n"
        };
        assert_eq!(problems(name, &spec), expected);
    }

    #[rstest]
    #[case::https("https://github.com/o/r.git")]
    #[case::https_bare("https://github.com/o/r")]
    #[case::trailing_slash("https://github.com/o/r/")]
    #[case::credentials("https://x-access-token:abc@github.com/o/r.git")]
    #[case::upper_host("https://GitHub.com/o/r")]
    #[case::scp("git@github.com:o/r.git")]
    #[case::ssh("ssh://git@github.com/o/r")]
    #[case::ssh_port("ssh://git@github.com:22/o/r.git")]
    #[case::query("https://github.com/o/r.git?ref=main")]
    #[case::spaces("  https://github.com/o/r  ")]
    fn one_repository_however_it_is_written(#[case] git: &str) {
        assert_eq!(repository_key(git), "github.com/o/r");
    }

    #[test]
    fn keeps_the_owner_and_repository_apart() {
        assert_ne!(
            repository_key("https://github.com/o/r"),
            repository_key("https://github.com/o/s")
        );
        assert_eq!(
            repository_key("https://gitlab.example.com/group/sub/r.git"),
            "gitlab.example.com/group/sub/r"
        );
    }
}
