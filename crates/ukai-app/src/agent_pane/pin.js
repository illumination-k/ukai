// Keeps the transcript `__ID__` at its newest entry until the user scrolls
// away from it, and tells the app when the view leaves the end and when it
// returns (`true`), so that only the end of a long transcript stays mounted
// (`ukai_core::tail`). A `column-reverse` scroll container would follow
// the end without a script, but WebKit then spends every frame, each key
// typed in the composer too, in proportion to the whole transcript.
const scroller = document.getElementById("__ID__");
const entries = scroller?.firstElementChild;
if (entries) {
  // How near its end the view still counts as following it.
  const NEAR = 8;
  let following = true;
  // The sizes the scroll position was last settled for.
  let height = 0;
  let view = 0;
  // The first entry mounted, and where it was: entries mounted above it
  // (the user asked for earlier ones) must not move what is being read.
  let first = null;
  let firstTop = 0;

  const settle = () => {
    // Not shown (its tab is not): there is nothing to scroll.
    if (scroller.clientHeight === 0) {
      return;
    }
    if (following) {
      scroller.scrollTop = scroller.scrollHeight;
    } else if (first?.isConnected) {
      scroller.scrollTop += first.offsetTop - firstTop;
    }
    first = entries.querySelector(":scope > .earlier + *");
    firstTop = first?.offsetTop ?? 0;
    height = scroller.scrollHeight;
    view = scroller.clientHeight;
  };

  scroller.addEventListener(
    "scroll",
    () => {
      // A scroll the layout made, before `settle` saw the new sizes, is not
      // the user's.
      if (scroller.scrollHeight !== height || scroller.clientHeight !== view) {
        return;
      }
      const now = height - scroller.scrollTop - view <= NEAR;
      if (now !== following) {
        following = now;
        dioxus.send(following);
      }
    },
    { passive: true },
  );

  // The entries grow as the agent writes; the scroller's own size changes
  // with the composer, the window, and its tab being shown.
  const sizes = new ResizeObserver(settle);
  sizes.observe(scroller);
  sizes.observe(entries);
}
