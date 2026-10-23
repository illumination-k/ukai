//! Browser panes: a web page served on a port, shown in a web view of its
//! own laid over the pane, with a bar to move around in it.
//!
//! The port is either this machine's (a dev server the user or a local agent
//! started) or one of a gateway agent's workspace, which the pane reaches
//! through a [`Tunnel`]: a listener on `127.0.0.1` carrying each connection
//! to the pod (see [`ukai_acp::ports`]). The page then has an ordinary local
//! origin, so its absolute paths and a dev server's hot-reload socket work.
//!
//! The page is untrusted like a report, and confined the same way: a child
//! web view with no IPC handler and no storage that outlives it (see
//! [`crate::report_pane`]). Unlike a report it may go anywhere on the web;
//! only links that are not web pages, and new windows, leave for the
//! default browser.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use dioxus::desktop::wry::{NewWindowResponse, Rect, WebView, WebViewBuilder};
use dioxus::prelude::*;
use ukai_acp::ports::preview_path;
use ukai_acp::{PaneWorkspace, Tunnel};
use ukai_core::{PaneId, ProjectId, ReportSource};

use crate::child_view::{ChildView, View, use_child_view};
use crate::icons::{Icon, Svg};
use crate::ports;
use crate::projects::AppSignals;
use crate::remote_reports::Workspace;
use crate::report_pane::open_external;

/// How often the bar follows the page's address.
const FOLLOW: Duration = Duration::from_millis(500);

#[component]
pub fn BrowserView(
    pane: PaneId,
    project: ProjectId,
    port: u16,
    path: String,
    source: ReportSource,
) -> Element {
    match source {
        // `localhost`, not `127.0.0.1`: a dev server bound to `::1` only is
        // as local as one on IPv4, and the web view tries both.
        ReportSource::Project => rsx! {
            BrowserSurface {
                key: "{pane}-{port}-{path}",
                origin: format!("http://localhost:{port}"),
                path,
                tunnelled: false,
            }
        },
        ReportSource::Agent { agent, workspace } => rsx! {
            TunnelledBrowser {
                key: "{pane}-{agent}-{port}-{path}",
                project,
                agent,
                workspace,
                port,
                path,
            }
        },
    }
}

/// A page on a port of a gateway agent's workspace: a tunnel to it, held as
/// long as the pane shows it, and the page through that.
#[component]
fn TunnelledBrowser(
    project: ProjectId,
    agent: String,
    workspace: Option<PaneWorkspace>,
    port: u16,
    path: String,
) -> Element {
    let app = use_context::<AppSignals>();
    let tunnel = use_resource(move || {
        let workspace = Workspace {
            project: project.clone(),
            agent: agent.clone(),
            workspace: workspace.clone(),
        };
        async move {
            let endpoint = ports::endpoint(&app.configs.peek(), &workspace)?;
            Tunnel::open(endpoint, port)
                .await
                .map(Rc::new)
                .map_err(|error| format!("cannot listen on this machine: {error}"))
        }
    });
    let opened = tunnel.read().clone();
    match opened {
        None => rsx! {
            div { class: "pane-body report-error",
                p { "Opening a tunnel to the workspace…" }
            }
        },
        Some(Err(error)) => rsx! {
            div { class: "pane-body report-error",
                p { "Cannot reach this port." }
                pre { class: "config-error", "{error}" }
            }
        },
        Some(Ok(tunnel)) => rsx! {
            BrowserSurface {
                key: "{tunnel.local_port()}",
                // `127.0.0.1`: the tunnel listens on IPv4 only, and
                // `localhost` could reach another server on `::1`.
                origin: format!("http://127.0.0.1:{}", tunnel.local_port()),
                path,
                tunnelled: true,
                tunnel: Some(TunnelHandle(tunnel)),
            }
        },
    }
}

/// Keeps a tunnel open while the surface showing it is mounted.
#[derive(Clone)]
struct TunnelHandle(Rc<Tunnel>);

impl PartialEq for TunnelHandle {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// The bar and the web view under it, starting at `path` on `origin`.
#[component]
fn BrowserSurface(
    origin: String,
    path: String,
    tunnelled: bool,
    #[props(default)] tunnel: Option<TunnelHandle>,
) -> Element {
    let app = use_context::<AppSignals>();
    // Held only to keep the tunnel open as long as this is mounted.
    let _tunnel = tunnel;
    let committed = use_hook(|| Rc::new(Cell::new(false)));
    let ChildView {
        view,
        mut host,
        failure,
    } = use_child_view(use_memo(move || app.covered()), {
        let start = format!("{origin}{path}");
        let committed = committed.clone();
        move |bounds, window| build(&start, &committed, bounds, window)
    });
    let mut address = use_signal(|| path.clone());
    let mut editing = use_signal(|| false);

    // Show where the page went, unless the user is typing over it.
    use_future({
        let view = view.clone();
        let origin = origin.clone();
        let committed = committed.clone();
        move || {
            let view = view.clone();
            let origin = origin.clone();
            let committed = committed.clone();
            async move {
                loop {
                    tokio::time::sleep(FOLLOW).await;
                    if let Some(url) = page_url(&view, &committed)
                        && !*editing.peek()
                    {
                        let shown = shown_address(&origin, &url);
                        if *address.peek() != shown {
                            address.set(shown);
                        }
                    }
                }
            }
        }
    });

    let script = {
        let view = view.clone();
        move |js: &'static str| {
            if let Some(webview) = view.borrow().as_ref() {
                let _ = webview.evaluate_script(js);
            }
        }
    };
    let back = script.clone();
    let forward = script;
    let reload = view.clone();
    let navigate = {
        let view = view.clone();
        let origin = origin.clone();
        move |typed: String| {
            if let (Some(url), Some(webview)) =
                (target_url(&origin, &typed), view.borrow().as_ref())
            {
                let _ = webview.load_url(&url);
            }
        }
    };
    let external = {
        let view = view.clone();
        let origin = origin.clone();
        move || {
            let url = page_url(&view, &committed).unwrap_or_else(|| origin.clone());
            open_external(&url);
        }
    };
    let host_label = origin.trim_start_matches("http://").to_owned();
    let external_title = if tunnelled {
        "Open in the browser (the tunnel stays open while this pane does)"
    } else {
        "Open in the browser"
    };

    rsx! {
        div { class: "pane-body browser",
            div { class: "browser-bar",
                button {
                    class: "icon",
                    title: "Back",
                    onclick: move |_| back("history.back()"),
                    Svg { icon: Icon::Back }
                }
                button {
                    class: "icon",
                    title: "Forward",
                    onclick: move |_| forward("history.forward()"),
                    Svg { icon: Icon::Chevron }
                }
                button {
                    class: "icon",
                    title: "Reload",
                    onclick: move |_| {
                        if let Some(webview) = reload.borrow().as_ref() {
                            let _ = webview.reload();
                        }
                    },
                    Svg { icon: Icon::Reload }
                }
                span { class: "browser-origin", title: "{origin}", "{host_label}" }
                input {
                    class: "browser-address",
                    spellcheck: false,
                    value: "{address}",
                    onfocus: move |_| editing.set(true),
                    onblur: move |_| editing.set(false),
                    oninput: move |event| address.set(event.value()),
                    onkeydown: move |event| {
                        if event.key() == Key::Enter {
                            navigate(address.peek().clone());
                            editing.set(false);
                        }
                    },
                }
                button {
                    class: "icon",
                    title: external_title,
                    onclick: move |_| external(),
                    Svg { icon: Icon::External }
                }
            }
            div {
                class: "browser-host",
                onmounted: move |event| host.set(Some(event.data())),
                if let Some(error) = failure() {
                    pre { class: "config-error", "{error}" }
                }
            }
        }
    }
}

/// A child web view of `window` showing `url`, with no IPC handler. It sets
/// `committed` once a page has started loading in it.
fn build(
    url: &str,
    committed: &Rc<Cell<bool>>,
    bounds: Rect,
    window: &dioxus::desktop::tao::window::Window,
) -> dioxus::desktop::wry::Result<WebView> {
    let committed = committed.clone();
    WebViewBuilder::new()
        .with_url(url)
        .with_on_page_load_handler(move |_, _| committed.set(true))
        .with_bounds(bounds)
        .with_incognito(true)
        .with_devtools(true)
        .with_navigation_handler(|url| allow_navigation(&url))
        .with_new_window_req_handler(|url, _| {
            open_external(&url);
            NewWindowResponse::Deny
        })
        .build_as_child(window)
}

/// The address of the page in `view`, once one has `committed`. Before that
/// WebKit has none, and wry's `url` unwraps it: a first load that fails, as
/// through a tunnel to a port nothing serves, would panic the app.
fn page_url(view: &View, committed: &Cell<bool>) -> Option<String> {
    if !committed.get() {
        return None;
    }
    view.borrow().as_ref()?.url().ok()
}

/// Web pages load in place, wherever they are; anything else (`mailto:`, an
/// app's own scheme) goes to the system.
fn allow_navigation(url: &str) -> bool {
    if ["http://", "https://", "about:", "data:", "blob:"]
        .iter()
        .any(|scheme| url.starts_with(scheme))
    {
        return true;
    }
    open_external(url);
    false
}

/// What the bar shows for `url`: its path on `origin`, or the whole URL once
/// the page has left it.
fn shown_address(origin: &str, url: &str) -> String {
    match url.strip_prefix(origin) {
        Some("") => "/".to_owned(),
        Some(path) if path.starts_with(['/', '?', '#']) => path.to_owned(),
        _ => url.to_owned(),
    }
}

/// Where `typed` in the bar leads: a full web URL as it is, else a path on
/// `origin`.
fn target_url(origin: &str, typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.starts_with("http://") || typed.starts_with("https://") {
        return Some(typed.to_owned());
    }
    let path = if typed.starts_with('/') {
        typed.to_owned()
    } else {
        format!("/{typed}")
    };
    preview_path(&path).map(|path| format!("{origin}{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bar_shows_the_path_on_the_pages_own_server() {
        let origin = "http://127.0.0.1:5173";
        assert_eq!(shown_address(origin, "http://127.0.0.1:5173/"), "/");
        assert_eq!(shown_address(origin, "http://127.0.0.1:5173"), "/");
        assert_eq!(
            shown_address(origin, "http://127.0.0.1:5173/docs?q=1"),
            "/docs?q=1"
        );
        // Another port of the same host is somewhere else.
        assert_eq!(
            shown_address(origin, "http://127.0.0.1:51730/"),
            "http://127.0.0.1:51730/"
        );
        assert_eq!(
            shown_address(origin, "https://accounts.example.com/login"),
            "https://accounts.example.com/login"
        );
    }

    #[test]
    fn the_bar_goes_where_the_user_typed() {
        let origin = "http://localhost:3000";
        assert_eq!(
            target_url(origin, "/admin").as_deref(),
            Some("http://localhost:3000/admin")
        );
        assert_eq!(
            target_url(origin, "admin?x=1").as_deref(),
            Some("http://localhost:3000/admin?x=1")
        );
        assert_eq!(
            target_url(origin, "").as_deref(),
            Some("http://localhost:3000/")
        );
        assert_eq!(
            target_url(origin, "https://example.com/").as_deref(),
            Some("https://example.com/")
        );
        // Not a way to another host under the origin's name.
        assert_eq!(target_url(origin, "//evil.example/"), None);
    }

    #[test]
    fn only_web_pages_load_in_the_pane() {
        assert!(allow_navigation("http://127.0.0.1:5173/"));
        assert!(allow_navigation("https://example.com/"));
        assert!(allow_navigation("about:blank"));
    }
}
