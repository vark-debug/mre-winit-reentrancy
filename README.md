# winit#3992 MRE — macOS re-entrancy abort

A minimal, self-contained reproduction of the macOS re-entrancy panic in **winit 0.30**
([rust-windowing/winit#3992](https://github.com/rust-windowing/winit/issues/3992)), as hit in the
wild by three sibling Rust applications:

- storytold/filmcraft [#2018](https://github.com/storytold/filmcraft/issues/2018) (fixed by
  [PR #2075](https://github.com/storytold/filmcraft/pull/2075))
- storytold/gridcraft [#2042](https://github.com/storytold/gridcraft/issues/2042)
- storytold/figcraft [#250](https://github.com/storytold/figcraft/issues/250) (fixed by the
  upstream commit `94ce2a36`)

One window, two buttons:

- **red button (BUGGY: Sync Dialog)** — opens `rfd::FileDialog::pick_file` *inside* the
  `WindowEvent` handler: the shape that aborts.
- **green button (FIXED: Async Sheet)** — opens an `rfd::AsyncFileDialog` completion-handler
  sheet begun on the main thread and awaited on a worker thread; the frame loop keeps running.

All on-screen text is a hand-typeset PNG embedded in the binary — the only dependencies are
`winit =0.30.13`, `rfd =0.17.2`, `softbuffer` (rendering) and `png` (decoding that image). No GUI
toolkit sits between the reproduction and winit.

## The bug

```
winit-0.30.13/src/platform_impl/macos/event_handler.rs:135:17:
    tried to handle event while another event is currently being handled
core/src/panicking.rs:225:5:
    panic in a function that cannot unwind
→ SIGABRT (exit code 134)
```

While the outer event is being handled, winit's `maybe_queue_event` wraps the deferred
`handle_event` call in a block posted via `CFRunLoopPerformBlock` on the **common modes**. A nested
run loop then executes that block **re-entrantly**, tripping winit's re-entrancy guard. Because the
block closure is `panic_cannot_unwind`, the panic bypasses the application's `catch_unwind` and
aborts the process — the app's panic hook never runs.

The most common source of the nested run loop is a synchronous `rfd::FileDialog` (the panel's
`run_modal`), but a Finder **drag-tracking session** hits the same path: the crash stacks show
`CoreDragMessageHandler → NSCoreDragTrackingProc → -[NSRunLoop runUntilDate:]` executing the
deferred block.

## Reproducing

Observed affected: macOS 26.5.x, 26.6.2 and 27.0.1 (Apple Silicon).

```sh
cargo run
```

1. Click the **red** button to open the synchronous file dialog.
2. Drag a file from Finder **over the exposed part of the application window** and onto the open
   dialog. Repeat a few times — this is a race; in our runs it aborted within about four attempts
   (earlier attempts survive and return `false`).
3. The **green** button (async sheet) does not abort under the same treatment.

Expected terminal output for the abort:

```text
[buggy] opening the sync dialog inside window_event...
thread 'main' panicked at winit-0.30.13/src/platform_impl/macos/event_handler.rs:135:17:
tried to handle event while another event is currently being handled
thread 'main' panicked at core/src/panicking.rs:225:5:
panic in a function that cannot unwind
```

### The window-size condition

**The application window must be larger than the file dialog to reproduce.** If the panel covers
the window entirely, drags land on the dialog itself and no window event is queued — the race
cannot happen, on any macOS version. Drags must cross a part of the window that stays visible
behind the dialog. This is why the bug looks "flaky" and why it was hard to triage from single
crash reports.

## Fixing it at the application level

Show file dialogs as `rfd::AsyncFileDialog` completion-handler sheets instead of running the panel
modally: create the future on the main thread (an AppKit requirement), `block_on` it on a worker
thread, and feed the answer back through a channel that the frame loop polls — see the green
button in `src/main.rs`, and the production implementations in
[filmcraft PR #2075](https://github.com/storytold/filmcraft/pull/2075) and figcraft's `94ce2a36`.

Caveat: `rfd`'s async API is not a complete escape — `ModalFuture` silently falls back to the
blocking modal path when the application is not running yet or no window exists.

A winit-level fix (deferring the event instead of panicking, or not marking the block
`panic_cannot_unwind`) would protect every application at once; see
[winit#3992](https://github.com/rust-windowing/winit/issues/3992).

## Status in winit 0.31.0-beta.3

This branch (`winit-0.31-beta`) also runs against `winit =0.31.0-beta.3` — the guard was
**moved but not removed**:

| | winit 0.30.13 | winit 0.31.0-beta.3 |
|---|---|---|
| panic location | `winit/src/platform_impl/macos/event_handler.rs:135` | `winit-common/src/event_handler.rs:138` |
| `panic_cannot_unwind` | yes → **SIGABRT (exit 134)** | **removed** → catchable panic (exit 101) |
| re-entrancy still panics | yes | yes |

The macOS backend was split into `winit-appkit` + `winit-common` in 0.31, and the guard now
lives in `winit-common`'s `handle()` — re-entrant execution of the queued closure still happens
(the nested run loop still runs it), it still panics, but the panic is a normal unwinding one
that an application's `catch_unwind` can survive. The `panic_cannot_unwind` attribute was
dropped, which turns the unrecoverable abort into a recoverable error. Deferring the event
instead of panicking (the cleaner fix) is still not implemented.

```text
thread 'main' panicked at winit-common-0.31.0-beta.3/src/event_handler.rs:138:17:
tried to handle event while another event is currently being handled
(exit code 101)
```

## License

MIT
