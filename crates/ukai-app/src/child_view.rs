//! A native child web view laid over an element of the app, for content the
//! app does not trust: a report, a page in a browser pane.
//!
//! The view is native, so it sits above the app's DOM: [`use_child_view`]
//! keeps it over its host element by polling the element's position, hides
//! it while something of the DOM is drawn over the pane (`covered`), and
//! drops it on unmount (tab change, split, close).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use dioxus::desktop::tao::window::Window;
use dioxus::desktop::wry::dpi::{LogicalPosition, LogicalSize};
use dioxus::desktop::wry::{Rect, WebView};
use dioxus::prelude::*;

/// How often the view follows its host's position.
const POLL: Duration = Duration::from_millis(150);

/// The web view once built, shared with whatever else drives it.
pub type View = Rc<RefCell<Option<WebView>>>;

/// A child view and where it goes: set `host` from the host element's
/// `onmounted`; `failure` says why the view could not be built.
#[derive(Clone)]
pub struct ChildView {
    pub view: View,
    pub host: Signal<Option<Rc<MountedData>>>,
    pub failure: Signal<Option<String>>,
}

/// Keep a web view over the host element, built by `build` the first time
/// the element has a place. The first `build` is the one used: key the
/// component by what it shows.
pub fn use_child_view(
    covered: Signal<bool>,
    build: impl Fn(Rect, &Window) -> dioxus::desktop::wry::Result<WebView> + 'static,
) -> ChildView {
    let view = use_hook(|| Rc::new(RefCell::new(None::<WebView>)));
    let host = use_signal(|| None::<Rc<MountedData>>);
    let mut failure = use_signal(|| None::<String>);
    let build = use_hook(|| Rc::new(build));

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

    // Keep the view over the host, which moves with every split, tab
    // change, and window resize.
    use_future({
        let view = view.clone();
        move || {
            let view = view.clone();
            let build = build.clone();
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
                                None => match build(bounds, &window.window) {
                                    Ok(webview) => {
                                        let _ = webview.set_visible(visible);
                                        *slot = Some(webview);
                                    }
                                    Err(error) => {
                                        tracing::warn!(%error, "cannot open a web view");
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

    ChildView {
        view,
        host,
        failure,
    }
}
