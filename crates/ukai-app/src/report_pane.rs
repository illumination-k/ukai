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
//! over its body by polling the body's position, and drops it on unmount.

use std::borrow::Cow;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use dioxus::desktop::wry::dpi::{LogicalPosition, LogicalSize};
use dioxus::desktop::wry::http::{Request, Response, header};
use dioxus::desktop::wry::{NewWindowResponse, Rect, WebView, WebViewBuilder};
use dioxus::html::geometry::PixelsRect;
use dioxus::prelude::*;
use ukai_core::report::{self, ReportFile};
use ukai_core::{PaneContent, PaneId, ProjectId};

use crate::icons::{Icon, Svg};
use crate::layout::{launch_target, open_report};
use crate::projects::{AppSignals, project_context};

const SCHEME: &str = "ukai-report";

/// Where `ukai-report` URLs live: wry maps custom protocols to `http` on
/// Windows and Android.
const ORIGIN: &str = if cfg!(any(target_os = "windows", target_os = "android")) {
    "http://ukai-report.localhost"
} else {
    "ukai-report://localhost"
};

/// How often the view follows the pane's position and checks the file.
const POLL: Duration = Duration::from_millis(150);

#[component]
pub fn ReportView(pane: PaneId, project: ProjectId, path: PathBuf) -> Element {
    let app = use_context::<AppSignals>();
    let report =
        project_context(&app.projects.read(), &app.configs.read(), &project).and_then(|context| {
            ReportFile::open(&context.root, &context.config.reports, &path)
                .map_err(|error| error.to_string())
        });
    match report {
        Ok(report) => rsx! {
            ReportSurface { key: "{pane}-{report.file().display()}", report }
        },
        Err(error) => rsx! {
            div { class: "pane-body report-error",
                p { "Cannot show this report." }
                pre { class: "config-error", "{error}" }
            }
        },
    }
}

/// The element the child web view covers.
#[component]
fn ReportSurface(report: ReportFile) -> Element {
    let view = use_hook(|| Rc::new(RefCell::new(None::<WebView>)));
    let mut host = use_signal(|| None::<Rc<MountedData>>);
    let failure = use_signal(|| None::<String>);

    use_drop({
        let view = view.clone();
        move || drop(view.borrow_mut().take())
    });

    use_future(move || follow(view.clone(), report.clone(), host, failure));

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

/// Keep the child view over `host` and reload it when the report changes,
/// until the view cannot be opened.
async fn follow(
    view: Rc<RefCell<Option<WebView>>>,
    report: ReportFile,
    host: Signal<Option<Rc<MountedData>>>,
    mut failure: Signal<Option<String>>,
) {
    let window = dioxus::desktop::window();
    let mut placed = None;
    let mut modified = report.modified();
    loop {
        let element = host.peek().clone();
        if let Some(element) = element
            && let Ok(rect) = element.get_client_rect().await
        {
            let key = Some((rect, window.window.inner_size()));
            if placed != key {
                placed = key;
                if let Err(error) = place(&view, &report, rect, &window.window) {
                    tracing::warn!(%error, "cannot open a report view");
                    failure.set(Some(error.to_string()));
                    return;
                }
            }
        }
        let now = report.modified();
        if now != modified {
            modified = now;
            if let Some(webview) = view.borrow().as_ref() {
                let _ = webview.reload();
            }
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Move the child view over `rect`, opening it the first time.
fn place(
    view: &RefCell<Option<WebView>>,
    report: &ReportFile,
    rect: PixelsRect,
    window: &dioxus::desktop::tao::window::Window,
) -> dioxus::desktop::wry::Result<()> {
    let bounds = Rect {
        position: LogicalPosition::new(rect.origin.x, rect.origin.y).into(),
        size: LogicalSize::new(rect.size.width, rect.size.height).into(),
    };
    let mut slot = view.borrow_mut();
    match slot.as_ref() {
        Some(webview) => {
            let _ = webview.set_bounds(bounds);
            let _ = webview.set_visible(rect.size.width > 0.0 && rect.size.height > 0.0);
        }
        None => *slot = Some(build(report, bounds, window)?),
    }
    Ok(())
}

/// A child web view of `window` showing `report`, with no IPC handler.
fn build(
    report: &ReportFile,
    bounds: Rect,
    window: &dioxus::desktop::tao::window::Window,
) -> dioxus::desktop::wry::Result<WebView> {
    let served = report.clone();
    WebViewBuilder::new()
        .with_url(format!("{ORIGIN}{}", report.entry()))
        .with_bounds(bounds)
        .with_incognito(true)
        .with_custom_protocol(SCHEME.to_owned(), move |_, request| {
            respond(&served, &request)
        })
        .with_navigation_handler(|url| allow_navigation(&url))
        .with_new_window_req_handler(|url, _| {
            open_external(&url);
            NewWindowResponse::Deny
        })
        .build_as_child(window)
}

fn respond(report: &ReportFile, request: &Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
    let target = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str());
    let reply = report.serve(target);
    let mut response = Response::builder()
        .status(reply.status)
        .header(header::CONTENT_TYPE, reply.content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if let Some(policy) = report.content_security_policy() {
        response = response.header(header::CONTENT_SECURITY_POLICY, policy);
    }
    response
        .body(Cow::<'static, [u8]>::Owned(reply.body))
        .unwrap_or_else(|_| Response::new(Cow::Borrowed(&[][..])))
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
            open_report(app, pane, &project, path);
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
            if !watched {
                p { class: "muted hint",
                    "List reports here with "
                    code { "[reports] dirs" }
                    " in ukai.toml."
                }
            } else if found.read().is_empty() {
                p { class: "muted hint", "No reports yet." }
            }
            for path in found() {
                {
                    let shown = open.iter().find(|(_, p)| *p == path).map(|(pane, _)| *pane);
                    let name = path
                        .file_name()
                        .map_or_else(
                            || path.display().to_string(),
                            |n| n.to_string_lossy().into_owned(),
                        );
                    let dir = path.parent().map(|d| d.display().to_string()).unwrap_or_default();
                    let project = project.clone();
                    rsx! {
                        div {
                            key: "{path.display()}",
                            class: if shown.is_some() { "row report-row open" } else { "row report-row" },
                            title: "{path.display()}",
                            onclick: move |_| {
                                if let Some(pane) = shown {
                                    let mut layout = app.layout;
                                    layout.write().focus(pane);
                                } else if let Some(pane) = launch_target(app) {
                                    open_report(app, pane, &project, path.clone());
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
            }
        }
    }
}

/// How often the sidebar rescans the report directories.
const RESCAN: Duration = Duration::from_secs(2);

/// Report panes of `project`, with the path each shows.
fn layout_open_reports(layout: &ukai_core::Layout, project: &ProjectId) -> Vec<(PaneId, PathBuf)> {
    layout
        .contents()
        .filter_map(|(pane, content)| match content {
            PaneContent::Report { project: p, path } if p == project => Some((pane, path.clone())),
            _ => None,
        })
        .collect()
}
