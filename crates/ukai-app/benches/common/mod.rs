//! What the benches share.

/// An agent message as long and as varied as a typical one.
pub const MESSAGE: &str = r#"## What changed

The layout is now **placed** rather than nested, so `Node::frames` decides
where each pane goes:

- splitting keeps every pane where it is in the DOM
- moving a pane changes only its `style`
- resizing changes only the frames

```rust
let frames = layout.active_tab().root.frames();
for (pane, frame) in frames.panes {
    println!("{pane}: {frame:?}");
}
```

| Operation | Before | After |
| --------- | ------ | ----- |
| split     | remount | place |
| move      | remount | place |

See [the design](docs/design.md) for the rest.
"#;
