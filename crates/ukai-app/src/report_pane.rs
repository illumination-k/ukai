//! Report panes: an agent-written HTML page or image, shown in a web view of
//! its own laid over the pane.
//!
//! Reports are untrusted. wry exposes its IPC handler to every frame of the
//! app's web view and Dioxus does not check which frame a message came from,
//! so a report in a sandboxed `<iframe>` could still forge UI events (answer
//! a permission request, close a project). Each report pane instead gets a
//! separate child web view with no IPC handler and no storage that outlives
//! it, which loads only its own files through the `ukai-report` protocol
//! (see [`ukai_core::report`]).
//!
//! The child view is native, so it sits above the app's DOM; see
//! [`crate::child_view`] for how the pane keeps it in place.
//!
//! A report of a gateway agent's workspace is in a pod, not on this machine,
//! so the same protocol handler fetches it through the gateway instead of
//! reading a file ([`ukai_acp::reports`]). Everything the view is confined by
//! — no IPC handler, no storage, the content security policy, one web view
//! per report — is the same either way; only where the bytes come from
//! differs, and what may be read is then the relay's decision.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use dioxus::desktop::wry::http::{Request, Response, header};
use dioxus::desktop::wry::{
    NewWindowResponse, Rect, RequestAsyncResponder, WebView, WebViewBuilder,
};
use dioxus::prelude::*;
use ukai_acp::ReportsEndpoint;
use ukai_acp::reports::ReportsError;
use ukai_core::markdown::LINK_SCHEMES;
use ukai_core::publication::Sessions;
use ukai_core::report::{self, CONFINED_CSP, IMAGE_VIEW_QUERY, ReportFile, ReportKind};
use ukai_core::sidebar::{Fold, Node};
use ukai_core::{PaneContent, PaneId, ProjectId, ReportSource};

use crate::child_view::{ChildView, use_child_view};
use crate::follow::{self, Workspace};
use crate::icons::{Icon, Svg};
use crate::layout::{close_pane, launch_target, open_report};
use crate::projects::{AppSignals, project_context};
use crate::remote_reports::{self, Listing};
use crate::schedules::{local_now, when_unix};
use crate::sidebar::Head;

const SCHEME: &str = "ukai-report";

/// Where `ukai-report` URLs live: wry maps custom protocols to `http` on
/// Windows and Android.
const ORIGIN: &str = if cfg!(any(target_os = "windows", target_os = "android")) {
    "http://ukai-report.localhost"
} else {
    "ukai-report://localhost"
};

/// How often a local report is checked for changes: one `stat`, so often.
const WATCH_LOCAL: Duration = Duration::from_millis(150);

/// How often a report in a pod is: one request through the gateway.
const WATCH_REMOTE: Duration = Duration::from_secs(2);

/// What a report pane reads from.
#[derive(Debug, Clone, PartialEq)]
enum Source {
    /// A file under the project root on this machine.
    Local(ReportFile),
    /// A file in the checkout of a gateway agent's workspace.
    Remote(Remote),
}

/// A report served from a pod: the workspace's endpoint and the file's path
/// in its checkout. The URL path space is that checkout's root, so a
/// report's relative assets resolve as they do on disk and the relay refuses
/// whatever lies outside the report directories.
#[derive(Debug, Clone, PartialEq)]
struct Remote {
    endpoint: ReportsEndpoint,
    path: PathBuf,
    kind: ReportKind,
    allow_network: bool,
}

impl Source {
    /// The path (and query) the web view starts at.
    fn entry(&self) -> String {
        match self {
            Self::Local(report) => report.entry(),
            Self::Remote(remote) => {
                let mut entry = report::url_path(&remote.path);
                if remote.kind == ReportKind::Image {
                    entry.push('?');
                    entry.push_str(IMAGE_VIEW_QUERY);
                }
                entry
            }
        }
    }

    fn content_security_policy(&self) -> Option<&'static str> {
        match self {
            Self::Local(report) => report.content_security_policy(),
            Self::Remote(remote) => (!remote.allow_network).then_some(CONFINED_CSP),
        }
    }

    /// Tells one pane's view from another's, and one report from the next.
    fn key(&self) -> String {
        match self {
            Self::Local(report) => report.file().display().to_string(),
            Self::Remote(remote) => format!("{}{}", remote.endpoint.base(), remote.path.display()),
        }
    }

    fn watch_interval(&self) -> Duration {
        match self {
            Self::Local(_) => WATCH_LOCAL,
            Self::Remote(_) => WATCH_REMOTE,
        }
    }

    /// Something that changes when the report does, to reload the view after
    /// the agent rewrites it: the file's modification time, or the `ETag` the
    /// gateway sends.
    async fn stamp(&self) -> Option<String> {
        match self {
            Self::Local(report) => report
                .modified()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|since| since.as_nanos().to_string()),
            Self::Remote(remote) => remote
                .endpoint
                .stamp(&report::url_path(&remote.path))
                .await
                .ok()
                .flatten(),
        }
    }
}

#[component]
pub fn ReportView(
    pane: PaneId,
    project: ProjectId,
    path: PathBuf,
    source: ReportSource,
) -> Element {
    let app = use_context::<AppSignals>();
    let covered = use_memo(move || app.covered());
    let opened = open_source(app, &project, &path, &source);
    match opened {
        // Dioxus honours a key only among siblings of a list, so the surface
        // goes in a list of one: a pane opening another report then builds a
        // new web view rather than keeping the first.
        Ok(source) => rsx! {
            for source in [source] {
                ReportSurface { key: "{pane}-{source.key()}", source, covered }
            }
        },
        Err(error) => rsx! {
            div { class: "pane-body report-error",
                p { "Cannot show this report." }
                pre { class: "config-error", "{error}" }
            }
        },
    }
}

/// Open what a pane's content names: a file of the project, or a file of the
/// workspace a gateway agent works in.
fn open_source(
    app: AppSignals,
    project: &ProjectId,
    path: &Path,
    source: &ReportSource,
) -> Result<Source, String> {
    let context = project_context(&app.projects.read(), &app.configs.read(), project)?;
    match source {
        ReportSource::Project => ReportFile::open(&context.root, &context.config.reports, path)
            .map(Source::Local)
            .map_err(|error| error.to_string()),
        ReportSource::Agent { agent, workspace } => {
            let kind = ReportKind::of(path)
                .ok_or_else(|| format!("{} is neither HTML nor an image", path.display()))?;
            let endpoint = follow::endpoint(
                &app.configs.read(),
                &Workspace {
                    project: project.clone(),
                    agent: agent.clone(),
                    workspace: workspace.clone(),
                },
                remote_reports::resolve,
            )?;
            Ok(Source::Remote(Remote {
                endpoint,
                path: path.to_owned(),
                kind,
                // The project's own setting: a report shown here is as
                // trusted as one written here, whoever wrote it.
                allow_network: context.config.reports.allow_network,
            }))
        }
    }
}

/// Save a copy of a report where the user chooses. For a gateway agent's
/// workspace this is the way to get the file onto this machine at all: it is
/// in a pod, or in the copy the gateway kept of one. Only the file itself is
/// saved, not what it loads.
pub fn download_report(app: AppSignals, project: &ProjectId, path: &Path, source: &ReportSource) {
    let opened = open_source(app, project, path, source);
    let name = path.file_name().map_or_else(
        || "report".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    spawn(async move {
        let dialog = rfd::AsyncFileDialog::new()
            .set_title("Save report")
            .set_file_name(&name);
        let saved = match opened {
            Ok(source) => {
                let Some(target) = dialog.save_file().await else {
                    return;
                };
                save(&source, target.path()).await
            }
            Err(problem) => Err(problem),
        };
        if let Err(problem) = saved {
            tracing::warn!(%problem, "cannot save a report");
            rfd::AsyncMessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("Cannot save this report")
                .set_description(problem)
                .show()
                .await;
        }
    });
}

/// Write the report `source` names to `target`.
async fn save(source: &Source, target: &Path) -> Result<(), String> {
    let body = match source {
        Source::Local(report) => std::fs::read(report.file())
            .map_err(|error| format!("{}: {error}", report.file().display()))?,
        Source::Remote(remote) => {
            let file = remote
                .endpoint
                .fetch(&report::url_path(&remote.path))
                .await
                .map_err(|error| match error {
                    ReportsError::Gone => "the agent's workspace is not running anymore, so \
                                           its reports cannot be read"
                        .to_owned(),
                    error => error.to_string(),
                })?;
            if file.status != 200 {
                return Err(format!("the gateway answered {}", file.status));
            }
            file.body
        }
    };
    std::fs::write(target, body).map_err(|error| format!("{}: {error}", target.display()))
}

/// Delete a report once the user confirms: the file in the project, or for a
/// gateway agent's workspace the one in its pod and in the copy the gateway
/// keeps. Only the file itself goes, not what it loads. The panes showing it
/// close, and `done` runs, e.g. to take it off a listing before the next
/// scan does.
///
/// Asking and failing are native dialogs: a report's web view is drawn over
/// the app's DOM.
pub fn delete_report(
    app: AppSignals,
    project: &ProjectId,
    path: &Path,
    source: &ReportSource,
    done: impl FnOnce() + 'static,
) {
    let opened = open_source(app, project, path, source);
    let (project, path, source) = (project.clone(), path.to_owned(), source.clone());
    spawn(async move {
        let opened = match opened {
            Ok(opened) => opened,
            Err(problem) => return cannot_delete(problem).await,
        };
        let place = if source.is_project() {
            "The file is deleted from the project."
        } else {
            "It is deleted from the agent's workspace and from the gateway's copy."
        };
        let confirmed = rfd::AsyncMessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("Delete this report?")
            .set_description(format!(
                "{}\n\n{place} This cannot be undone.",
                path.display()
            ))
            .set_buttons(rfd::MessageButtons::OkCancelCustom(
                DELETE.to_owned(),
                "Cancel".to_owned(),
            ))
            .show()
            .await;
        let confirmed = match confirmed {
            rfd::MessageDialogResult::Ok | rfd::MessageDialogResult::Yes => true,
            rfd::MessageDialogResult::Custom(button) => button == DELETE,
            rfd::MessageDialogResult::No | rfd::MessageDialogResult::Cancel => false,
        };
        if !confirmed {
            return;
        }
        if let Err(problem) = remove(&opened).await {
            return cannot_delete(problem).await;
        }
        // Nothing below waits: what follows may unmount the component this
        // runs for, which would stop it halfway.
        if let Some(workspace) = Workspace::at(&project, &source) {
            let mut reports = app.reports;
            if let Some(Listing::Reports(paths, _) | Listing::Saved { paths, .. }) =
                reports.write().get_mut(&workspace)
            {
                paths.retain(|listed| *listed != path);
            }
        }
        done();
        let showing: Vec<PaneId> = layout_open_reports(&app.layout.peek(), &project)
            .into_iter()
            .filter(|(_, open_path, open_source)| *open_path == path && *open_source == source)
            .map(|(pane, _, _)| pane)
            .collect();
        for pane in showing {
            close_pane(app, pane);
        }
    });
}

async fn cannot_delete(problem: String) {
    tracing::warn!(%problem, "cannot delete a report");
    rfd::AsyncMessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title("Cannot delete this report")
        .set_description(problem)
        .show()
        .await;
}

/// The button that confirms deleting a report.
const DELETE: &str = "Delete";

/// Delete the report `source` names from where it is.
async fn remove(source: &Source) -> Result<(), String> {
    match source {
        Source::Local(report) => report.remove().map_err(|error| error.to_string()),
        Source::Remote(remote) => remote
            .endpoint
            .remove(&report::url_path(&remote.path))
            .await
            .map_err(|error| match error {
                ReportsError::Gone => "the agent's workspace is not running anymore and the \
                                       gateway kept no copy of its reports"
                    .to_owned(),
                ReportsError::Status(403) => {
                    "this file is outside the workspace's report directories".to_owned()
                }
                ReportsError::Status(405) => {
                    "this gateway, or the relay in its agent image, cannot delete reports yet; update it"
                        .to_owned()
                }
                error => error.to_string(),
            }),
    }
}

/// The element the child web view covers. While `covered` is set something
/// in the DOM is drawn over the pane, so the view hides.
#[component]
fn ReportSurface(source: Source, covered: Memo<bool>) -> Element {
    let ChildView {
        view,
        mut host,
        failure,
    } = use_child_view(covered, {
        let source = source.clone();
        move |bounds, window| build(&source, bounds, window)
    });

    // Follow the report itself: an agent that rewrites it reloads the view.
    use_future({
        let view = view.clone();
        let source = source.clone();
        move || {
            let view = view.clone();
            let source = source.clone();
            async move {
                let mut stamp = source.stamp().await;
                loop {
                    tokio::time::sleep(source.watch_interval()).await;
                    let now = source.stamp().await;
                    if now == stamp {
                        continue;
                    }
                    stamp = now;
                    if let Some(webview) = view.borrow().as_ref() {
                        let _ = webview.reload();
                    }
                }
            }
        }
    });

    rsx! {
        div {
            class: "pane-body report-host",
            onmounted: move |event| host.set(Some(event.data())),
            if let Some(error) = failure() {
                pre { class: "config-error", "{error}" }
            }
        }
    }
}

/// A child web view of `window` showing `source`, with no IPC handler.
fn build(
    source: &Source,
    bounds: Rect,
    window: &dioxus::desktop::tao::window::Window,
) -> dioxus::desktop::wry::Result<WebView> {
    let served = source.clone();
    // A report in a pod is fetched, so every request is answered
    // asynchronously; the web view's thread never waits on the network.
    let runtime = tokio::runtime::Handle::try_current().ok();
    WebViewBuilder::new()
        .with_url(format!("{ORIGIN}{}", source.entry()))
        .with_bounds(bounds)
        .with_incognito(true)
        .with_asynchronous_custom_protocol(SCHEME.to_owned(), move |_, request, responder| {
            answer(&served, runtime.as_ref(), &request, responder);
        })
        .with_navigation_handler(|url| allow_navigation(&url))
        .with_new_window_req_handler(|url, _| {
            open_external(&url);
            NewWindowResponse::Deny
        })
        .build_as_child(window)
}

/// Answer one request from a report's web view. A file on this machine is
/// read and answered at once; a file in a pod is fetched on `runtime`, so
/// the view waits on the request rather than on the UI thread.
fn answer(
    source: &Source,
    runtime: Option<&tokio::runtime::Handle>,
    request: &Request<Vec<u8>>,
    responder: RequestAsyncResponder,
) {
    let target = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .to_owned();
    match source {
        Source::Local(report) => {
            let reply = report.serve(&target);
            responder.respond(reply_response(
                reply.status,
                reply.content_type,
                reply.body,
                source.content_security_policy(),
            ));
        }
        Source::Remote(remote) => {
            let Some(runtime) = runtime else {
                responder.respond(problem_response(
                    500,
                    "this report cannot be fetched: the app has no runtime to do it on",
                ));
                return;
            };
            let remote = remote.clone();
            let policy = source.content_security_policy();
            runtime.spawn(async move {
                responder.respond(fetched(&remote, &target, policy).await);
            });
        }
    }
}

/// One request answered from the pod the workspace runs in.
async fn fetched(
    remote: &Remote,
    path_and_query: &str,
    policy: Option<&'static str>,
) -> Response<Cow<'static, [u8]>> {
    let (path, query) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let path = path.split_once('#').map_or(path, |(path, _)| path);
    if query == IMAGE_VIEW_QUERY
        && remote.kind == ReportKind::Image
        && report::is_url_path(path, &remote.path)
    {
        let page = report::image_page(&remote.path);
        return reply_response(200, "text/html; charset=utf-8", page.into_bytes(), policy);
    }
    match remote.endpoint.fetch(path).await {
        Ok(file) => {
            let content_type = file
                .content_type
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            let mut response = Response::builder()
                .status(file.status)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CACHE_CONTROL, "no-store")
                .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
            if let Some(policy) = policy {
                response = response.header(header::CONTENT_SECURITY_POLICY, policy);
            }
            response
                .body(Cow::Owned(file.body))
                .unwrap_or_else(|_| Response::new(Cow::Borrowed(&[][..])))
        }
        Err(ReportsError::Gone) => problem_response(
            410,
            "the agent's workspace is not running anymore, so its reports cannot be read",
        ),
        Err(error) => {
            tracing::warn!(%error, "cannot read a report from the gateway");
            problem_response(502, &error.to_string())
        }
    }
}

fn reply_response(
    status: u16,
    content_type: &str,
    body: Vec<u8>,
    policy: Option<&'static str>,
) -> Response<Cow<'static, [u8]>> {
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if let Some(policy) = policy {
        response = response.header(header::CONTENT_SECURITY_POLICY, policy);
    }
    response
        .body(Cow::Owned(body))
        .unwrap_or_else(|_| Response::new(Cow::Borrowed(&[][..])))
}

/// What the view shows instead of the report when it cannot be read. Plain
/// text: the message is the app's, but it is rendered by the untrusted view.
fn problem_response(status: u16, message: &str) -> Response<Cow<'static, [u8]>> {
    reply_response(
        status,
        "text/plain; charset=utf-8",
        message.as_bytes().to_vec(),
        Some(CONFINED_CSP),
    )
}

/// Pages of the report load in place; web links open in the browser instead
/// of replacing the report.
fn allow_navigation(url: &str) -> bool {
    if url.starts_with(ORIGIN)
        || ["about:", "data:", "blob:"]
            .iter()
            .any(|s| url.starts_with(s))
    {
        return true;
    }
    open_external(url);
    false
}

pub fn open_external(url: &str) {
    if LINK_SCHEMES.iter().any(|scheme| url.starts_with(scheme))
        && let Err(error) = webbrowser::open(url)
    {
        tracing::warn!(%error, "cannot open a link");
    }
}

/// `path` relative to `root` if it lies inside, e.g. for a file an agent
/// reported writing.
pub fn project_relative(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let path = path.canonicalize().ok()?;
    path.strip_prefix(&root).ok().map(Path::to_owned)
}

/// Ask for an HTML file or image in `project` and show it in `pane`, or
/// where [`launch_target`] puts it. A file outside the project opens to an
/// error in the pane rather than being served.
pub fn pick_report(app: AppSignals, pane: Option<PaneId>, project: ProjectId) {
    let Some(root) = app.projects.peek().get(&project).map(|p| p.root.clone()) else {
        return;
    };
    spawn(async move {
        let Some(file) = rfd::AsyncFileDialog::new()
            .set_title("Open report or image")
            .set_directory(&root)
            .add_filter("Reports and images", REPORT_EXTENSIONS)
            .pick_file()
            .await
        else {
            return;
        };
        let path = project_relative(&root, file.path()).unwrap_or_else(|| file.path().to_owned());
        if let Some(pane) = pane.or_else(|| launch_target(app)) {
            open_report(app, pane, &project, path, ReportSource::Project);
        }
    });
}

const REPORT_EXTENSIONS: &[&str] = &[
    "html", "htm", "svg", "png", "jpg", "jpeg", "gif", "webp", "avif", "bmp",
];

/// The reports of one workspace of a project in the sidebar: for this
/// machine, what `reports.dirs` holds, rescanned while shown; for a gateway
/// workspace, what [`remote_reports::Follow`] last listed. Newest first.
#[component]
pub fn ReportGroup(project: ProjectId, place: ReportSource, depth: u8) -> Element {
    let app = use_context::<AppSignals>();
    let mut found = use_signal(Vec::<PathBuf>::new);
    let local = place.is_project();
    let watched = local && {
        let configs = app.configs.read();
        match configs.get(&project) {
            Some(Ok(config)) => !config.reports.dirs.is_empty(),
            _ => false,
        }
    };
    use_future({
        let project = project.clone();
        move || {
            let project = project.clone();
            async move {
                // Only this machine's directories are scanned here.
                if !local {
                    return;
                }
                loop {
                    let scan = project_context(&app.projects.peek(), &app.configs.peek(), &project)
                        .map(|context| report::list(&context.root, &context.config.reports))
                        .unwrap_or_default();
                    if *found.peek() != scan {
                        found.set(scan);
                    }
                    tokio::time::sleep(RESCAN).await;
                }
            }
        }
    });
    let listing = match Workspace::at(&project, &place) {
        None => Listing::Reports(found(), Sessions::default()),
        Some(workspace) => app
            .reports
            .read()
            .get(&workspace)
            .cloned()
            .unwrap_or(Listing::Reports(Vec::new(), Sessions::default())),
    };
    let fold = Fold::new(
        &project,
        Node::Reports {
            place: place.clone(),
        },
    );
    let open_now = app.folds.read().is_open(&fold);
    let open = layout_open_reports(&app.layout.read(), &project);
    // Whose reports go first: the focused agent pane's, if it works here.
    let session = ukai_core::publication::focused_session(&app.layout.read(), &project, &place);
    let count = match &listing {
        Listing::Reports(paths, _) | Listing::Saved { paths, .. } if !paths.is_empty() => {
            paths.len().to_string()
        }
        _ => String::new(),
    };
    let inner = depth + 1;

    rsx! {
        Head {
            fold,
            depth,
            icon: Icon::Document,
            label: "Reports",
            meta: count,
            if local {
                button {
                    class: "icon",
                    title: "Open report or image…",
                    onclick: {
                        let project = project.clone();
                        move |event: MouseEvent| {
                            event.stop_propagation();
                            pick_report(app, None, project.clone());
                        }
                    },
                    Svg { icon: Icon::Plus }
                }
            }
        }
        if open_now {
            match listing {
                Listing::NotRunning => rsx! {
                    p { class: "muted hint tree-hint", style: "--depth: {inner}", "The workspace is not running." }
                },
                Listing::Failed(problem) => rsx! {
                    pre { class: "config-error tree-hint", style: "--depth: {inner}", "{problem}" }
                },
                Listing::Reports(paths, _) if paths.is_empty() => rsx! {
                    if local && !watched {
                        p { class: "muted hint tree-hint", style: "--depth: {inner}",
                            "List them here with "
                            code { "[reports] dirs" }
                            " in ukai.toml."
                        }
                    } else {
                        p { class: "muted hint tree-hint", style: "--depth: {inner}", "No reports yet." }
                    }
                },
                Listing::Saved { paths, synced, sessions } => rsx! {
                    p {
                        class: "muted hint tree-hint",
                        style: "--depth: {inner}",
                        title: "The workspace is not running; these are the copies the gateway kept.",
                        "Saved {when_unix(synced as i64, local_now())} · not running"
                    }
                    ReportList {
                        project: project.clone(),
                        place: place.clone(),
                        paths,
                        sessions,
                        session: session.clone(),
                        open: open.clone(),
                        depth: inner,
                        found,
                    }
                },
                Listing::Reports(paths, sessions) => rsx! {
                    ReportList {
                        project: project.clone(),
                        place: place.clone(),
                        paths,
                        sessions,
                        session: session.clone(),
                        open: open.clone(),
                        depth: inner,
                        found,
                    }
                },
            }
        }
    }
}

/// A workspace's reports: those the focused session published first, when
/// it works here and any session published anything, then the rest, which
/// other sessions published or nobody did.
#[component]
fn ReportList(
    project: ProjectId,
    place: ReportSource,
    paths: Vec<PathBuf>,
    sessions: Sessions,
    session: Option<String>,
    open: Vec<(PaneId, PathBuf, ReportSource)>,
    depth: u8,
    found: Signal<Vec<PathBuf>>,
) -> Element {
    let (mine, rest) = match &session {
        Some(session) if !sessions.is_empty() => {
            let (mine, rest) = sessions.split(&paths, session);
            (
                Some(mine.into_iter().cloned().collect::<Vec<_>>()),
                rest.into_iter().cloned().collect::<Vec<_>>(),
            )
        }
        _ => (None, paths),
    };
    let row = |path: PathBuf| {
        rsx! {
            ReportRow {
                key: "{path.display()}",
                project: project.clone(),
                label: sessions.title(&path).map(str::to_owned),
                path,
                source: place.clone(),
                open: open.clone(),
                depth,
                found,
            }
        }
    };
    rsx! {
        if let Some(mine) = mine {
            if mine.is_empty() {
                p { class: "muted hint tree-hint", style: "--depth: {depth}",
                    "This session has published no report yet."
                }
            }
            for path in mine {
                {row(path)}
            }
            if !rest.is_empty() {
                p {
                    class: "muted hint tree-hint",
                    style: "--depth: {depth}",
                    title: "Reports other sessions published here, or that nobody published",
                    "Other sessions"
                }
            }
        }
        for path in rest {
            {row(path)}
        }
    }
}

/// One report in the sidebar: focuses the pane already showing it, else
/// opens it where [`launch_target`] puts it. `found` is the scan of this
/// machine's reports the row may come from, which deleting it takes it off.
#[component]
fn ReportRow(
    project: ProjectId,
    path: PathBuf,
    /// What the session that published it called it.
    #[props(default)]
    label: Option<String>,
    source: ReportSource,
    open: Vec<(PaneId, PathBuf, ReportSource)>,
    depth: u8,
    found: Signal<Vec<PathBuf>>,
) -> Element {
    let app = use_context::<AppSignals>();
    let shown = open
        .iter()
        .find(|(_, open_path, open_source)| *open_path == path && *open_source == source)
        .map(|(pane, _, _)| *pane);
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let dir = path
        .parent()
        .map(|dir| dir.display().to_string())
        .unwrap_or_default();
    let saved = (project.clone(), path.clone(), source.clone());
    let deleted = saved.clone();
    let hint = match &label {
        Some(label) => format!("{label} · {}", path.display()),
        None => path.display().to_string(),
    };
    // A published report goes by the title its session gave it.
    let name = label.unwrap_or(name);

    rsx! {
        div {
            class: if shown.is_some() { "row report-row open" } else { "row report-row" },
            style: "--depth: {depth}",
            title: "{hint}",
            onclick: move |_| {
                if let Some(pane) = shown {
                    let mut layout = app.layout;
                    layout.write().focus(pane);
                } else if let Some(pane) = launch_target(app) {
                    open_report(app, pane, &project, path.clone(), source.clone());
                }
            },
            span { class: "row-icon",
                Svg { icon: Icon::Document, size: 12 }
            }
            span { class: "name", "{name}" }
            span { class: "meta", "{dir}" }
            span { class: "row-actions",
                button {
                    class: "icon",
                    title: "Save a copy…",
                    onclick: move |event: MouseEvent| {
                        event.stop_propagation();
                        download_report(app, &saved.0, &saved.1, &saved.2);
                    },
                    Svg { icon: Icon::Download }
                }
                button {
                    class: "icon",
                    title: "Delete…",
                    onclick: move |event: MouseEvent| {
                        event.stop_propagation();
                        let (project, path, source) = deleted.clone();
                        let local = source.is_project();
                        let mut found = found;
                        delete_report(
                            app,
                            &project,
                            &path.clone(),
                            &source,
                            move || {
                                if local {
                                    found.write().retain(|listed| *listed != path);
                                }
                            },
                        );
                    },
                    Svg { icon: Icon::Trash }
                }
            }
        }
    }
}

/// How often the sidebar rescans this machine's report directories.
const RESCAN: Duration = Duration::from_secs(2);

/// Report panes of `project`, with what each shows.
fn layout_open_reports(
    layout: &ukai_core::Layout,
    project: &ProjectId,
) -> Vec<(PaneId, PathBuf, ReportSource)> {
    layout
        .contents()
        .filter_map(|(pane, content)| match content {
            PaneContent::Report {
                project: p,
                path,
                source,
            } if p == project => Some((pane, path.clone(), source.clone())),
            _ => None,
        })
        .collect()
}
