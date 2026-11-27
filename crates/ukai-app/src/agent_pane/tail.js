// Tells the app when the view of the transcript `__ID__` leaves its end and
// when it returns (`true`), so that only the end of a long transcript stays
// mounted (`ukai_core::tail`).
const scroller = document.getElementById("__ID__");
if (scroller) {
  // How near its end the view still counts as being there.
  const NEAR = 8;
  let following = true;
  scroller.addEventListener(
    "scroll",
    () => {
      // A `column-reverse` scroller starts at its end: 0 there, and
      // negative above it.
      const now = Math.abs(scroller.scrollTop) <= NEAR;
      if (now !== following) {
        following = now;
        dioxus.send(following);
      }
    },
    { passive: true },
  );
}
