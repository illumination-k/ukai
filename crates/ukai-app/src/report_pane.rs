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
//! The child view is native, so it sits above the app's DOM: the pane keeps it
//! over its body by polling the body's position, hides it while the command
//! palette is open, and drops it on unmount.
//!
//! A report of a gateway agent's workspace is in a pod, not on this machine,
//! so the same protocol handler fetches it through the gateway instead of
//! reading a file ([`ukai_acp::reports`]). Everything the view is confined by
//! — no IPC handler, no storage, the content security policy, one web view
//! per report — is the same either way; only where the bytes come from
//! differs, and what may be read is then the relay's decision.

use std::borrow::Cow;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use dioxus::desktop::wry::dpi::{LogicalPosition, LogicalSize};
use dioxus::desktop::wry::http::{Request, Response, header};
use dioxus::desktop::wry::{
    NewWindowResponse, Rect, RequestAsyncResponder, WebView, WebViewBuilder,
};
use dioxus::prelude::*;
use ukai_acp::ReportsEndpoint;
use ukai_acp::reports::ReportsError;
use ukai_core::report::{self, CONFINED_CSP, IMAGE_VIEW_QUERY, ReportFile, ReportKind};
use ukai_core::{PaneContent, PaneId, ProjectId, ReportSource};

use crate::icons::{Icon, Svg};
use crate::layout::{launch_target, open_report};
use crate::projects::{AppSignals, project_context};
use crate::remote_reports::{self, Listing, Workspace};

const SCHEME: &str = "ukai-report";

/// Where `ukai-report` URLs live: wry maps custom protocols to `http` on
/// Windows and Android.
const ORIGIN: &str = if cfg!(any(target_os = "windows", target_os = "android")) {
    "http://ukai-report.localhost"
} else {
    "ukai-report://localhost"
};

/// How often the view follows the pane's position.
const POLL: Duration = Duration::from_millis(150);

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
    let opened = open_source(app, &project, &path, &source);
    match opened {
        Ok(source) => rsx! {
            ReportSurface {
                key: "{pane}-{source.key()}",
                source,
                covered: app.palette,
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
            let endpoint = remote_reports::endpoint(
                &app.configs.read(),
                &Workspace {
                    project: project.clone(),
                    agent: agent.clone(),
                    workspace: workspace.clone(),
                },
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

/// The element the child web view covers. While `covered` is set something
/// in the DOM is drawn over the pane, so the view hides.
#[component]
fn ReportSurface(source: Source, covered: Signal<bool>) -> Element {
    let view = use_hook(|| Rc::new(RefCell::new(None::<WebView>)));
    let mut host = use_signal(|| None::<Rc<MountedData>>);
    let mut failure = use_signal(|| None::<String>);

    // Hide at once; the poll below shows the view again once uncovered.
    use_effect({
        let view = view.clone();
        move || {
            if covered()
                && let Some(webview) = view.borrow().as_ref()
            {
                let _ = webview.set_visible(false);
            }
        }
    });

    use_drop({
        let view = view.clone();
        move || drop(view.borrow_mut().take())
    });

    // Keep the view over the pane's body, which moves with every split,
    // tab change, and window resize.
    use_future({
        let view = view.clone();
        let source = source.clone();
        move || {
            let view = view.clone();
            let source = source.clone();
            async move {
                let window = dioxus::desktop::window();
                let mut placed = None;
                loop {
                    let element = host.peek().clone();
                    if let Some(element) = element
                        && let Ok(rect) = element.get_client_rect().await
                    {
                        let window_size = window.window.inner_size();
                        let hidden = *covered.peek();
                        let key = (
                            rect.origin.x,
                            rect.origin.y,
                            rect.size.width,
                            rect.size.height,
                            window_size,
                            hidden,
                        );
                        if placed != Some(key) {
                            placed = Some(key);
                            let bounds = Rect {
                                position: LogicalPosition::new(rect.origin.x, rect.origin.y).into(),
                                size: LogicalSize::new(rect.size.width, rect.size.height).into(),
                            };
                            let visible =
                                !hidden && rect.size.width > 0.0 && rect.size.height > 0.0;
                            let mut slot = view.borrow_mut();
                            match slot.as_ref() {
                                Some(webview) => {
                                    let _ = webview.set_bounds(bounds);
                                    let _ = webview.set_visible(visible);
                                }
                                None => match build(&source, bounds, &window.window) {
                                    Ok(webview) => {
                                        let _ = webview.set_visible(visible);
                                        *slot = Some(webview);
                                    }
                                    Err(error) => {
                                        tracing::warn!(%error, "cannot open a report view");
                                        failure.set(Some(error.to_string()));
                                        return;
                                    }
                                },
                            }
                        }
                    }
                    tokio::time::sleep(POLL).await;
                }
            }
        }
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

fn open_external(url: &str) {
    if ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
        && let Err(error) = webbrowser::open(url)
    {
        tracing::warn!(%error, "cannot open a link from a report");
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

/// The selected project's reports in the sidebar: what `reports.dirs` holds,
/// newest first, rescanned while shown.
#[component]
pub fn ReportList(project: ProjectId, name: String) -> Element {
    let app = use_context::<AppSignals>();
    let mut found = use_signal(Vec::<PathBuf>::new);
    let watched = {
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
    let open = layout_open_reports(&app.layout.read(), &project);
    let workspaces = remote_reports::of_project(app, &project);

    rsx! {
        div { class: "sidebar-section",
            div { class: "sidebar-header",
                span { class: "section-label", "Reports · {name}" }
                button {
                    class: "icon",
                    title: "Open report or image…",
                    onclick: {
                        let project = project.clone();
                        move |_| pick_report(app, None, project.clone())
                    },
                    Svg { icon: Icon::Plus }
                }
            }
            // The hint is about this project's own files, so it would only be
            // in the way once a workspace is listing its own below.
            if !watched && workspaces.is_empty() {
                p { class: "muted hint",
                    "List reports here with "
                    code { "[reports] dirs" }
                    " in ukai.toml."
                }
            } else if watched && found.read().is_empty() {
                p { class: "muted hint", "No reports yet." }
            }
            for path in found() {
                ReportRow {
                    key: "{path.display()}",
                    project: project.clone(),
                    path: path.clone(),
                    source: ReportSource::Project,
                    open: open.clone(),
                }
            }
            for (workspace, listing) in workspaces {
                div {
                    key: "{workspace.agent}-{workspace.workspace.as_ref().map(|w| w.key.clone()).unwrap_or_default()}",
                    class: "sidebar-subsection",
                    div { class: "sidebar-header",
                        span { class: "section-label", "In {workspace.label()}" }
                    }
                    match listing {
                        Listing::NotRunning => rsx! {
                            p { class: "muted hint", "The workspace is not running." }
                        },
                        Listing::Failed(problem) => rsx! {
                            pre { class: "config-error", "{problem}" }
                        },
                        Listing::Reports(paths) if paths.is_empty() => rsx! {
                            p { class: "muted hint", "No reports yet." }
                        },
                        Listing::Reports(paths) => rsx! {
                            for path in paths {
                                ReportRow {
                                    key: "{path.display()}",
                                    project: project.clone(),
                                    path: path.clone(),
                                    source: workspace.source(),
                                    open: open.clone(),
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

/// One report in the sidebar: focuses the pane already showing it, else
/// opens it where [`launch_target`] puts it.
#[component]
fn ReportRow(
    project: ProjectId,
    path: PathBuf,
    source: ReportSource,
    open: Vec<(PaneId, PathBuf, ReportSource)>,
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

    rsx! {
        div {
            class: if shown.is_some() { "row report-row open" } else { "row report-row" },
            title: "{path.display()}",
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
        }
    }
}

/// How often the sidebar rescans the report directories.
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
