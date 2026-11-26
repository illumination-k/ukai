//! GitHub credentials in a pod whose agent uses the gateway's GitHub App
//! (see "GitHub App" in `docs/design.md`). Nothing in the pod holds a
//! long-lived token: git's credential helper is `ukai-relay git-credential`
//! ([`credential`]), which asks the gateway each time, and `gh`, which reads
//! its token from its configuration, has the relay rewrite `hosts.yml` in
//! `GH_CONFIG_DIR` before the last one expires ([`keep_gh_fresh`]).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ukai_acp::github::{GitHubToken, TokenEndpoint};

/// The environment variable naming where to ask for tokens, set by the
/// gateway in both of the pod's containers.
pub const GATEWAY_ENV: &str = "UKAI_GITHUB_GATEWAY";

/// How long before a token expires `gh` is given the next one. The gateway
/// hands out tokens good for at least ten more minutes, so this is under
/// that.
const RENEW_BEFORE: Duration = Duration::from_secs(5 * 60);
/// How soon to ask again after the gateway did not answer.
const RETRY: Duration = Duration::from_secs(30);

/// The user name git sends with an installation token.
const USER: &str = "x-access-token";

/// What git asked a credential helper for, from its `key=value` lines.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Asked {
    pub protocol: Option<String>,
    pub host: Option<String>,
}

/// Reads git's request, up to its blank line or the end.
pub fn asked(input: &str) -> Asked {
    let mut asked = Asked::default();
    for line in input.lines() {
        if line.is_empty() {
            break;
        }
        match line.split_once('=') {
            Some(("protocol", value)) => asked.protocol = Some(value.to_owned()),
            Some(("host", value)) => asked.host = Some(value.to_owned()),
            _ => {}
        }
    }
    asked
}

/// Whether `asked` is for the repository's host over https, the only thing
/// the token is good for.
pub fn answers(asked: &Asked, host: &str) -> bool {
    asked.protocol.as_deref() == Some("https")
        && asked
            .host
            .as_deref()
            .is_some_and(|asked| asked.eq_ignore_ascii_case(host))
}

/// The helper's answer with `token`.
pub fn credential(token: &GitHubToken) -> String {
    format!("username={USER}\npassword={}\n", token.token)
}

/// `gh`'s `hosts.yml` holding `token` for its host.
pub fn hosts_yml(token: &GitHubToken) -> String {
    format!(
        "{}:\n    oauth_token: {}\n    user: {USER}\n    git_protocol: https\n",
        token.host, token.token
    )
}

/// How long to keep a token expiring at `expires_at` before renewing it, at
/// `now` (both in seconds since the Unix epoch). At least [`RETRY`], so a
/// token that is already stale is not asked for in a loop.
pub fn renew_in(expires_at: i64, now: i64) -> Duration {
    let left = u64::try_from(expires_at.saturating_sub(now)).unwrap_or(0);
    Duration::from_secs(left)
        .saturating_sub(RENEW_BEFORE)
        .max(RETRY)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_secs()).ok())
        .unwrap_or(0)
}

/// Write `contents` to `path` whole, readable by this user only, so `gh`
/// never reads half a file.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let partial = path.with_extension("partial");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&partial)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(partial, path)
}

/// Keep `dir/hosts.yml` holding a token from `endpoint`, renewed before it
/// expires, until the relay exits.
pub async fn keep_gh_fresh(endpoint: TokenEndpoint, dir: PathBuf) {
    let path = dir.join("hosts.yml");
    loop {
        let wait = match endpoint.token().await {
            Ok(token) => match write_private(&path, &hosts_yml(&token)) {
                Ok(()) => {
                    tracing::info!(repository = %token.repository, expires_at = token.expires_at, "gh has a fresh GitHub token");
                    renew_in(token.expires_at, unix_now())
                }
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "cannot give gh its GitHub token");
                    RETRY
                }
            },
            Err(error) => {
                tracing::warn!(%error, "cannot get a GitHub token for gh");
                RETRY
            }
        };
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn token() -> GitHubToken {
        GitHubToken {
            token: "ghs_abc".into(),
            expires_at: 10_000,
            host: "github.com".into(),
            repository: "o/r".into(),
        }
    }

    #[test]
    fn reads_what_git_asks_up_to_the_blank_line() {
        let input = "protocol=https\nhost=github.com\npath=o/r.git\n\nhost=evil.example\n";
        assert_eq!(
            asked(input),
            Asked {
                protocol: Some("https".into()),
                host: Some("github.com".into()),
            }
        );
    }

    #[rstest]
    #[case::the_repositorys_host("protocol=https\nhost=github.com\n", true)]
    #[case::any_case("protocol=https\nhost=GitHub.com\n", true)]
    #[case::another_host("protocol=https\nhost=gitlab.com\n", false)]
    #[case::plain_http("protocol=http\nhost=github.com\n", false)]
    #[case::no_host("protocol=https\n", false)]
    fn answers_only_for_the_repositorys_host(#[case] input: &str, #[case] expected: bool) {
        assert_eq!(answers(&asked(input), "github.com"), expected);
    }

    #[test]
    fn hands_git_and_gh_the_token() {
        assert_eq!(
            credential(&token()),
            "username=x-access-token\npassword=ghs_abc\n"
        );
        assert_eq!(
            hosts_yml(&token()),
            "github.com:\n    oauth_token: ghs_abc\n    user: x-access-token\n    git_protocol: https\n"
        );
        assert!(!format!("{:?}", token()).contains("ghs_abc"));
    }

    #[rstest]
    #[case::an_hour_left(3600, Duration::from_secs(3600) - RENEW_BEFORE)]
    #[case::nearly_stale(60, RETRY)]
    #[case::stale(-100, RETRY)]
    fn renews_before_the_token_expires(#[case] left: i64, #[case] expected: Duration) {
        assert_eq!(renew_in(1000 + left, 1000), expected);
    }

    #[test]
    fn writes_hosts_yml_for_this_user_only() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let path = dir.path().join("hosts.yml");
        write_private(&path, "a").unwrap_or_else(|e| panic!("{e}"));
        write_private(&path, "b").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(std::fs::read_to_string(&path).ok().as_deref(), Some("b"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o777);
            assert_eq!(mode.ok(), Some(0o600));
        }
    }
}
