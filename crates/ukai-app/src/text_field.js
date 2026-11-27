// Keys an input method takes stay in the web view. Every event the app
// listens to is a synchronous request to it, which the web view waits out
// before it goes on; three of them for each key of a composition (keydown,
// input, keyup) make typing through an input method lag. The app ignores
// those keys anyway, so they are stopped here, and it is told the field's
// value once the composition ends.
//
// xterm.js handles its own textarea, compositions included.
if (!window.ukaiComposition) {
  window.ukaiComposition = true;

  const field = (target) =>
    (target instanceof HTMLTextAreaElement ||
      target instanceof HTMLInputElement) &&
    !target.closest(".xterm");
  // WebKit sends the key that ends a composition after it, `isComposing`
  // false, but still with the key code of a key the input method took.
  const composing = (event) => event.isComposing || event.keyCode === 229;
  // The composer sizes its textarea from a copy of the draft, which the app
  // does not write while it is not told what is typed.
  const grow = (target) => {
    const box = target.parentElement;
    if (box?.classList.contains("composer-input")) {
      box.dataset.value = target.value;
    }
  };

  for (const type of ["keydown", "keyup", "input"]) {
    document.addEventListener(
      type,
      (event) => {
        if (!field(event.target) || !composing(event)) {
          return;
        }
        event.stopPropagation();
        if (type === "input") {
          grow(event.target);
        }
      },
      true,
    );
  }

  document.addEventListener(
    "compositionend",
    (event) => {
      const target = event.target;
      if (!field(target)) {
        return;
      }
      // Once the field holds what the composition left.
      setTimeout(() => {
        target.dispatchEvent(new InputEvent("input", { bubbles: true }));
      }, 0);
    },
    true,
  );
}
