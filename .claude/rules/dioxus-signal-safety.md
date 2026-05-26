---
description: When working on UI components, signal mutations, or Dioxus runtime interactions — required reading for any change under ui/src/components/ or ui/src/util.rs
globs:
  - ui/src/**/*.rs
---

# Dioxus WASM Signal Safety Rules

The UI runs as single-threaded WASM. Firefox mobile runs Dioxus signal
subscriber notifications synchronously during Drop, causing
`RefCell already borrowed` panics. These rules prevent re-entrant borrow
crashes.

## Always use `try_read()` for reactive signal reads

```rust
// WRONG — panics if signal is being written
let rooms = ROOMS.read();

// RIGHT — returns Err instead of panicking
let Ok(rooms) = ROOMS.try_read() else { return; };
```

**IMPORTANT:** In Dioxus 0.7.x, `try_read()` does NOT register signal
subscriptions when it returns `Err`. The subscription is registered only
on the success path (after the borrow succeeds). This means a `use_memo`
that hits `try_read() -> Err` will NOT be notified of future signal
changes — it permanently stops re-evaluating.

To mitigate: ensure signal mutations happen in clean execution contexts
(via `crate::util::defer()`) so `try_read()` never encounters a
concurrent borrow. Also, memos that read multiple signals (e.g.,
`CURRENT_ROOM.read()` + `ROOMS.try_read()`) get a backup subscription
from the non-try signal.

## Never call `spawn_local` inside a polled future

Use `safe_spawn_local()` (in `util.rs`) which defers via `setTimeout(0)`:

```rust
// WRONG — re-entrant Task::run() panic on Firefox at singlethread.rs:132
wasm_bindgen_futures::spawn_local(async { ... });

// RIGHT
crate::util::safe_spawn_local(async { ... });
```

## Never mutate signals inside `spawn_local` or event handlers

Signal mutations (`ROOMS.with_mut()`, `ROOMS.write()`,
`CURRENT_ROOM.write()`, etc.) must always be wrapped in
`crate::util::defer()` when called from `spawn_local` tasks or
synchronous event handlers (`onclick`, etc.). This is required for TWO
reasons:

1. **RefCell re-entrancy**: Signal write Drop handlers fire subscriber
   notifications synchronously. Those notifications poll memos that call
   `try_read()` on the same signal — panics if the write guard's
   RefCell borrow is still held. `setTimeout(0)` breaks the call stack
   so no borrows are active.

2. **Missing Dioxus scope**: `wasm_bindgen_futures::spawn_local` tasks
   run without a Dioxus scope on the `scope_stack`. Signal subscriber
   notifications call `current_scope_id()` which panics on an empty
   scope_stack (`runtime.rs:223`). Our `defer()` uses
   `runtime.in_scope(ScopeId::ROOT, f)` to push both the runtime and a
   root scope before executing the closure.

**IMPORTANT**: `defer()` depends on `capture_runtime()` being called at
app startup (in `App()` component). Without it, deferred closures have
no runtime to push and GlobalSignal access panics with "Must be called
from inside a Dioxus runtime."

```rust
// WRONG — panics at runtime.rs:223 (empty scope_stack) and/or
//         runtime.rs:280 (RefCell already borrowed)
spawn_local(async {
    ROOMS.with_mut(|rooms| { /* mutate */ });
});

// ALSO WRONG — onclick handlers trigger the same RefCell panic
onclick: move |_| {
    ROOMS.write().map.remove(&key);
};

// RIGHT — defer mutation to clean execution context with runtime+scope
spawn_local(async {
    // ... async work (signing, etc.) ...
    crate::util::defer(move || {
        ROOMS.with_mut(|rooms| { /* mutate */ });
        crate::components::app::mark_needs_sync(key);
    });
});

// RIGHT — onclick with defer
onclick: move |_| {
    crate::util::defer(move || {
        ROOMS.write().map.remove(&key);
    });
};
```

**Ordering caveat**: `defer()` schedules via `setTimeout(0)`, so the
closure runs asynchronously. Code after `defer()` executes BEFORE the
deferred closure. If you need data from a signal mutation for
subsequent code, extract it before deferring:

```rust
// WRONG — signing_keys will be empty because ROOMS merge hasn't happened yet
crate::util::defer(move || { ROOMS.with_mut(|r| r.merge(loaded_rooms)); });
let signing_keys = ROOMS.with(|r| /* read signing keys */); // reads pre-merge state!

// RIGHT — extract data before moving into defer
let signing_keys = loaded_rooms.iter().map(|r| r.signing_key()).collect();
crate::util::defer(move || { ROOMS.with_mut(|r| r.merge(loaded_rooms)); });
```

See `defer()` in `util.rs`, `capture_runtime()` in `util.rs`,
`mark_needs_sync()` in `app.rs`.

## Never use raw setTimeout for signal mutations

Always use `crate::util::defer()` instead of manual
`web_sys::window().set_timeout_with_callback()`. Our `defer()` pushes
the Dioxus runtime and root scope via
`runtime.in_scope(ScopeId::ROOT, f)`. Raw setTimeout runs without any
Dioxus context, so GlobalSignal access panics.

## Never defer signal clears in `use_effect`

Signal clears that the effect subscribes to must be synchronous.
Deferring causes an infinite loop (set remains non-empty → effect
re-runs → defers clear → effect re-runs...).

## Don't `use_memo` against non-signal values in an always-mounted component

The modals in `app.rs` (`MemberInfoModal`, `DmThreadModal`,
`InviteViaDmPickerModal`, etc.) are mounted unconditionally and only
return an empty element when inactive — the component instance, and all
its hooks, live for the whole app session and never reinitialise.

A `use_memo` recomputes only when a *signal it read* changes. If its
closure depends on a plain captured value (a destructured field of some
*other* signal, a prop, anything that is not itself a `Signal`), the
memo will keep handing back the value computed from the *first*
render's captured input — it is never told that input changed. In an
always-mounted modal this surfaces as stale content on reopen.

Compute such values inline in the render body instead (the component
re-renders when the signal driving its open/close state changes), or
reset per-open `use_signal` scratch state with a `use_effect` keyed on
that open/close signal. freenet/river#291 (the invite-via-DM picker
showing the previous invitee's name) was exactly this bug.
