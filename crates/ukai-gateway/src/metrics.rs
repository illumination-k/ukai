//! Metrics per pull request, as the gateway serves them (see "PR ごとの
//! メトリクス" in `docs/design.md`): `ukai.gateway.v1.WorkspaceMetrics` to
//! its agent pods, whose relays record each prompt turn as it ends, and
//! `ukai.gateway.v1.Metrics` to its clients, which sums an agent's turns per
//! branch and joins each branch with the pull request and the GitHub
//! Actions runs GitHub has for it. `ukai-store` keeps the turns
//! (`ukai.metrics.v1.MetricsStore`), by the workspace and agent the gateway
//! finds from the pod, never from the request.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Mutex;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::Channel;
use tonic::{Request, Response, Status};
use ukai_core::metrics::{self, Branch, Ci, PullRequest, ToolStat, Turn};
use ukai_proto::gateway::v1 as gw;
use ukai_proto::metrics::v1 as pb;
use ukai_proto::metrics::v1::metrics_store_client::MetricsStoreClient;

use crate::knowledge::{answer, request};
use crate::{Gateway, access};

/// How far back a dashboard looks when the client says nothing, and at
/// most, in days.
const DAYS_DEFAULT: u32 = 30;
const DAYS_MAX: u32 = 365;
/// Most branches asked about on GitHub per request, the most recently
/// active; the rest go without.
const GITHUB_BRANCHES_MAX: usize = 50;
/// GitHub requests in flight at once.
const GITHUB_CONCURRENCY: usize = 8;
/// How long an answer from GitHub is kept.
const GITHUB_TTL: Duration = Duration::from_secs(300);
const GITHUB_TIMEOUT: Duration = Duration::from_secs(20);

/// The metrics side of `ukai-store`, at the address the gateway was given.
#[derive(Clone)]
pub struct MetricsClient {
    client: MetricsStoreClient<Channel>,
    /// `Bearer <token>`, marked sensitive so it is never logged.
    token: MetadataValue<Ascii>,
}

impl MetricsClient {
    /// A client of the store at `url`; see [`crate::knowledge::connect`].
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        let (channel, token) = crate::knowledge::connect(url, token)?;
        Ok(Self {
            client: MetricsStoreClient::new(channel),
            token,
        })
    }

    async fn record(
        &self,
        workspace: &str,
        agent: &str,
        turn: pb::Turn,
    ) -> Result<pb::RecordResponse, Status> {
        let message = request(
            &self.token,
            pb::RecordRequest {
                workspace: workspace.to_owned(),
                agent: agent.to_owned(),
                turn: Some(turn),
            },
        );
        answer(self.client.clone().record(message).await)
    }

    async fn list(&self, agent: &str, since: i64) -> Result<Vec<(String, Turn)>, Status> {
        let message = request(
            &self.token,
            pb::ListRequest {
                agent: agent.to_owned(),
                since,
            },
        );
        let listed = answer(self.client.clone().list(message).await)?;
        Ok(listed
            .turns
            .into_iter()
            .filter_map(|recorded| Some((recorded.workspace, recorded.turn?.into())))
            .collect())
    }
}

/// `ukai.gateway.v1.WorkspaceMetrics`: an agent pod, known by its relay's
/// token.
pub struct PodMetrics {
    pub gateway: Arc<Gateway>,
}

#[tonic::async_trait]
impl gw::workspace_metrics_server::WorkspaceMetrics for PodMetrics {
    async fn record(
        &self,
        request: Request<pb::Turn>,
    ) -> Result<Response<pb::RecordResponse>, Status> {
        let store = self.gateway.metrics.as_ref().ok_or_else(|| {
            Status::unavailable(
                "this gateway keeps no store: set UKAI_STORE_URL and UKAI_STORE_TOKEN",
            )
        })?;
        let pod = crate::knowledge::pod_asking(&self.gateway, &request).await?;
        let agent = pod.agent.ok_or_else(|| {
            Status::failed_precondition("the pod does not say its agent; restart it")
        })?;
        let answered = store
            .record(&pod.workspace, &agent, request.into_inner())
            .await?;
        if !answered.problems.is_empty() {
            tracing::info!(workspace = %pod.workspace, problems = ?answered.problems, "refused a turn's metrics");
        }
        Ok(Response::new(answered))
    }
}

/// `ukai.gateway.v1.Metrics`, for clients.
pub struct ClientMetrics {
    pub gateway: Arc<Gateway>,
}

#[tonic::async_trait]
impl gw::metrics_server::Metrics for ClientMetrics {
    async fn branches(
        &self,
        request: Request<gw::BranchesRequest>,
    ) -> Result<Response<gw::BranchesResponse>, Status> {
        let headers = request.metadata().clone().into_headers();
        access::authorize(&self.gateway, &headers)?;
        let request = request.into_inner();
        let agent = access::agent(&self.gateway, &request.agent)?;
        let Some(store) = &self.gateway.metrics else {
            return Ok(Response::new(gw::BranchesResponse {
                branches: Vec::new(),
                problem: Some(
                    "this gateway keeps no store, so it records no metrics: set UKAI_STORE_URL and UKAI_STORE_TOKEN"
                        .to_owned(),
                ),
            }));
        };
        let days = match request.days {
            0 => DAYS_DEFAULT,
            days => days.min(DAYS_MAX),
        };
        let now = now_millis();
        let since = now - i64::from(days) * 86_400_000;
        let turns = store.list(&request.agent, since).await?;
        let mut branches = metrics::branches(&turns);
        let repo = github_repo(&agent.workspace.git);
        self.gateway
            .github
            .join(repo.as_deref(), &mut branches)
            .await;
        Ok(Response::new(gw::BranchesResponse {
            branches: branches.into_iter().map(Into::into).collect(),
            problem: None,
        }))
    }
}

fn now_millis() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// `owner/name` of a repository on github.com, from its clone URL.
pub fn github_repo(git: &str) -> Option<String> {
    let rest = git
        .strip_prefix("https://github.com/")
        .or_else(|| git.strip_prefix("http://github.com/"))
        .or_else(|| git.strip_prefix("git@github.com:"))
        .or_else(|| git.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    (parts.next().is_none() && valid(owner) && valid(name)).then(|| format!("{owner}/{name}"))
}

/// What GitHub says about one branch.
type Answer = Result<(Option<PullRequest>, Option<Ci>), String>;

/// GitHub's REST API, with what it said lately.
pub struct Github {
    http: reqwest::Client,
    api: String,
    /// Never logged.
    token: Option<String>,
    cache: Mutex<HashMap<(String, String), (Instant, Answer)>>,
}

impl Github {
    /// `api` is `https://api.github.com` unless GitHub Enterprise;
    /// `token` reads private repositories and raises the rate limit.
    pub fn new(api: String, token: Option<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(GITHUB_TIMEOUT)
            .user_agent("ukai-gateway")
            .build()
            .unwrap_or_default();
        Self {
            http,
            api: api.trim_end_matches('/').to_owned(),
            token,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Fill in the pull request and CI runs of each branch of `repo`.
    async fn join(&self, repo: Option<&str>, branches: &mut [Branch]) {
        let Some(repo) = repo else {
            for branch in branches.iter_mut() {
                branch.github_problem = Some("the agent's repository is not on GitHub".to_owned());
            }
            return;
        };
        let asked: Vec<(usize, String)> = branches
            .iter()
            .enumerate()
            .filter(|(_, branch)| !branch.branch.is_empty())
            .map(|(index, branch)| (index, branch.branch.clone()))
            .collect();
        let answers: Vec<(usize, Option<Answer>)> =
            futures::stream::iter(asked.into_iter().enumerate().map(
                |(rank, (index, name))| async move {
                    if rank >= GITHUB_BRANCHES_MAX {
                        return (index, None);
                    }
                    (index, Some(self.branch(repo, &name).await))
                },
            ))
            .buffer_unordered(GITHUB_CONCURRENCY)
            .collect()
            .await;
        for branch in branches.iter_mut() {
            branch.repo = Some(repo.to_owned());
        }
        for (index, answer) in answers {
            let branch = &mut branches[index];
            match answer {
                Some(Ok((pull_request, ci))) => {
                    branch.pull_request = pull_request;
                    branch.ci = ci;
                }
                Some(Err(problem)) => branch.github_problem = Some(problem),
                None => {
                    branch.github_problem = Some(format!(
                        "GitHub is asked about the {GITHUB_BRANCHES_MAX} most recently active branches only"
                    ));
                }
            }
        }
    }

    async fn branch(&self, repo: &str, branch: &str) -> Answer {
        let key = (repo.to_owned(), branch.to_owned());
        if let Some((at, answer)) = self.cache.lock().await.get(&key)
            && at.elapsed() < GITHUB_TTL
        {
            return answer.clone();
        }
        let answer = self.ask(repo, branch).await;
        let mut cache = self.cache.lock().await;
        cache.retain(|_, (at, _)| at.elapsed() < GITHUB_TTL);
        cache.insert(key, (Instant::now(), answer.clone()));
        answer
    }

    async fn ask(&self, repo: &str, branch: &str) -> Answer {
        let owner = repo.split('/').next().unwrap_or_default();
        let head = format!("{owner}:{branch}");
        let pulls = self
            .get(
                &format!("/repos/{repo}/pulls"),
                &[("head", head.as_str()), ("state", "all"), ("per_page", "5")],
            )
            .await?;
        let runs = self
            .get(
                &format!("/repos/{repo}/actions/runs"),
                &[("branch", branch), ("per_page", "100")],
            )
            .await?;
        Ok((parse_pull_request(&pulls), parse_ci(&runs)))
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, String> {
        let url = reqwest::Url::parse_with_params(&format!("{}{path}", self.api), query)
            .map_err(|error| format!("cannot ask GitHub at {}: {error}", self.api))?;
        let mut request = self
            .http
            .get(url)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("cannot reach GitHub: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(match status.as_u16() {
                401 => "GitHub refused the gateway's UKAI_GITHUB_TOKEN".to_owned(),
                403 | 429 => {
                    "GitHub's rate limit is spent; set UKAI_GITHUB_TOKEN on the gateway, or wait"
                        .to_owned()
                }
                404 => {
                    "GitHub does not show this repository; a private one needs UKAI_GITHUB_TOKEN"
                        .to_owned()
                }
                code => format!("GitHub answered {code}"),
            });
        }
        response
            .json()
            .await
            .map_err(|error| format!("GitHub answered something unreadable: {error}"))
    }
}

/// The time GitHub writes, in Unix milliseconds.
fn millis_at(value: Option<&Value>) -> Option<i64> {
    let at = OffsetDateTime::parse(value?.as_str()?, &Rfc3339).ok()?;
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok()
}

/// The newest pull request of GitHub's list of them.
fn parse_pull_request(pulls: &Value) -> Option<PullRequest> {
    let newest = pulls
        .as_array()?
        .iter()
        .max_by_key(|pull| millis_at(pull.get("created_at")).unwrap_or(0))?;
    let merged = millis_at(newest.get("merged_at"));
    let closed = millis_at(newest.get("closed_at"));
    let state = if merged.is_some() {
        "merged"
    } else if newest.get("state").and_then(Value::as_str) == Some("open") {
        "open"
    } else {
        "closed"
    };
    Some(PullRequest {
        number: newest.get("number")?.as_u64()?,
        title: newest
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        url: newest
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        state: state.to_owned(),
        created: millis_at(newest.get("created_at")).unwrap_or(0),
        merged,
        closed,
    })
}

/// GitHub's list of a branch's workflow runs, summed per workflow; none
/// when it has none.
fn parse_ci(runs: &Value) -> Option<Ci> {
    let runs = runs.get("workflow_runs")?.as_array()?;
    if runs.is_empty() {
        return None;
    }
    let mut ci = Ci::default();
    let mut workflows: HashMap<String, ToolStat> = HashMap::new();
    for run in runs {
        let name = run
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("workflow")
            .to_owned();
        let finished = run.get("status").and_then(Value::as_str) == Some("completed");
        let failed = matches!(
            run.get("conclusion").and_then(Value::as_str),
            Some("failure" | "timed_out" | "startup_failure")
        );
        let millis = if finished {
            match (
                millis_at(run.get("run_started_at")),
                millis_at(run.get("updated_at")),
            ) {
                (Some(start), Some(end)) => u64::try_from(end - start).unwrap_or(0),
                _ => 0,
            }
        } else {
            0
        };
        ci.runs += 1;
        ci.failed += u32::from(failed);
        ci.millis += millis;
        let workflow = workflows.entry(name.clone()).or_insert_with(|| ToolStat {
            name,
            ..ToolStat::default()
        });
        workflow.count += 1;
        workflow.failed += u32::from(failed);
        workflow.millis += millis;
    }
    let mut workflows: Vec<ToolStat> = workflows.into_values().collect();
    workflows.sort_by(|a, b| b.millis.cmp(&a.millis).then_with(|| a.name.cmp(&b.name)));
    ci.workflows = workflows;
    Some(ci)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    #[rstest]
    #[case("https://github.com/o/r", Some("o/r"))]
    #[case("https://github.com/o/r.git", Some("o/r"))]
    #[case("https://github.com/o/r/", Some("o/r"))]
    #[case("git@github.com:o/r.git", Some("o/r"))]
    #[case("ssh://git@github.com/o/r", Some("o/r"))]
    #[case("https://gitlab.com/o/r", None)]
    #[case("https://github.com/o", None)]
    #[case("https://github.com/o/r/tree/main", None)]
    #[case("https://github.com/o/r?x=1", None)]
    fn finds_the_github_repository(#[case] git: &str, #[case] expected: Option<&str>) {
        assert_eq!(github_repo(git).as_deref(), expected);
    }

    #[test]
    fn reads_the_newest_pull_request() {
        let pulls = json!([
            {"number": 3, "title": "old", "html_url": "u3", "state": "closed",
             "created_at": "2026-01-01T00:00:00Z", "closed_at": "2026-01-02T00:00:00Z", "merged_at": null},
            {"number": 7, "title": "new", "html_url": "u7", "state": "closed",
             "created_at": "2026-02-01T00:00:00Z", "closed_at": "2026-02-01T01:00:00Z",
             "merged_at": "2026-02-01T01:00:00Z"},
        ]);
        let pr = parse_pull_request(&pulls).unwrap_or_else(|| panic!("no pull request"));
        assert_eq!(
            (pr.number, pr.title.as_str(), pr.state.as_str()),
            (7, "new", "merged")
        );
        assert_eq!(pr.merged.map(|m| m - pr.created), Some(3_600_000));
        assert_eq!(metrics::lead_millis(&pr, 0), 3_600_000);
        assert!(parse_pull_request(&json!([])).is_none());
        let open = parse_pull_request(
            &json!([{"number": 1, "state": "open", "created_at": "2026-01-01T00:00:00Z"}]),
        );
        assert_eq!(open.map(|pr| pr.state), Some("open".to_owned()));
    }

    #[test]
    fn sums_the_workflow_runs() {
        let runs = json!({"total_count": 3, "workflow_runs": [
            {"name": "ci", "status": "completed", "conclusion": "success",
             "run_started_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:05:00Z"},
            {"name": "ci", "status": "completed", "conclusion": "failure",
             "run_started_at": "2026-01-01T01:00:00Z", "updated_at": "2026-01-01T01:01:00Z"},
            {"name": "e2e", "status": "in_progress", "conclusion": null,
             "run_started_at": "2026-01-01T02:00:00Z", "updated_at": "2026-01-01T02:10:00Z"},
        ]});
        let ci = parse_ci(&runs).unwrap_or_else(|| panic!("no runs"));
        assert_eq!((ci.runs, ci.failed, ci.millis), (3, 1, 360_000));
        assert_eq!(
            ci.workflows,
            [
                ToolStat {
                    name: "ci".into(),
                    count: 2,
                    failed: 1,
                    millis: 360_000
                },
                ToolStat {
                    name: "e2e".into(),
                    count: 1,
                    failed: 0,
                    millis: 0
                },
            ]
        );
        assert!(parse_ci(&json!({"workflow_runs": []})).is_none());
    }

    #[tokio::test]
    async fn a_repository_off_github_is_said_so() {
        let github = Github::new("http://127.0.0.1:9".into(), None);
        let mut branches = vec![Branch {
            branch: "b".into(),
            ..Branch::default()
        }];
        github.join(None, &mut branches).await;
        assert!(branches[0].github_problem.is_some());
        assert!(branches[0].pull_request.is_none());
    }

    #[tokio::test]
    async fn a_branchless_turn_asks_github_nothing() {
        // Nothing listens on the discard port; asking would leave a problem.
        let github = Github::new("http://127.0.0.1:9".into(), None);
        let mut branches = vec![Branch::default()];
        github.join(Some("o/r"), &mut branches).await;
        assert_eq!(branches[0].repo.as_deref(), Some("o/r"));
        assert!(branches[0].github_problem.is_none());
    }
}
