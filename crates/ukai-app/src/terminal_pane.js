// The view of a terminal pane: xterm.js (bundled in assets/vendor/xterm, no
// network) in the element `__ID__`. It renders the shell's bytes as text
// cells, never as HTML. Messages to Rust: {kind: "resize", cols, rows},
// {kind: "input", data} (UTF-8 text), {kind: "binary", data} (one byte per
// char). From Rust: {data: <base64>}, {focus: true}.
const id = "__ID__";
const ready = () => window.Terminal && window.FitAddon && document.getElementById(id);
while (!ready()) {
  await new Promise((resolve) => setTimeout(resolve, 20));
}
const host = document.getElementById(id);
const style = getComputedStyle(host);
const color = (name, fallback) => style.getPropertyValue(name).trim() || fallback;
// xterm.js 6 puts its exports on the global object; the fit addon nests
// its own under `FitAddon`.
const Xterm = window.Terminal.Terminal ?? window.Terminal;
const Fit = window.FitAddon.FitAddon ?? window.FitAddon;
const term = new Xterm({
  cursorBlink: true,
  fontFamily: color("--mono", "monospace"),
  fontSize: 13,
  scrollback: 10000,
  theme: {
    background: color("--terminal-bg", "#0a0a0a"),
    foreground: color("--terminal-fg", "#fafafa"),
    cursor: color("--terminal-fg", "#fafafa"),
    selectionBackground: color("--terminal-selection", "rgba(128, 128, 128, 0.35)"),
  },
});
const fit = new Fit();
term.loadAddon(fit);
term.open(host);
const refit = () => {
  if (host.offsetWidth > 0 && host.offsetHeight > 0) {
    try {
      fit.fit();
    } catch (_) {}
  }
};
refit();
term.onData((data) => dioxus.send({ kind: "input", data }));
term.onBinary((data) => dioxus.send({ kind: "binary", data }));
term.onResize(({ cols, rows }) => dioxus.send({ kind: "resize", cols, rows }));
dioxus.send({ kind: "resize", cols: term.cols, rows: term.rows });
const observer = new ResizeObserver(refit);
observer.observe(host);
let disposed;
const gone = new Promise((resolve) => (disposed = resolve));
window.ukaiTerminals ??= {};
window.ukaiTerminals[id] = { dispose: () => disposed() };
const decode = (base64) => Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
for (;;) {
  const message = await Promise.race([dioxus.recv(), gone.then(() => null)]);
  if (message === null || message === undefined) break;
  if (message.data !== undefined) term.write(decode(message.data));
  if (message.focus) term.focus();
}
observer.disconnect();
term.dispose();
delete window.ukaiTerminals[id];
