---
description: When working on UI components, signal mutations, or Dioxus runtime interactions — required reading for any change under ui/src/components/ or ui/src/util.rs
globs:
  - ui/src/**/*.rs
---

# Dioxus WASM Signal Safety Rules

The UI runs as single-threaded WASM, where a re-entrant signal borrow is a
`RefCell already borrowed` panic rather than a benign contention. These rules
prevent that, and prevent the subscription latch below.

Note on the mechanism: earlier revisions of this file said Dioxus fires
subscriber notifications synchronously during a write guard's Drop, with the
borrow still held. That is not what dioxus 0.7.9 does — `WriteLock`'s borrow
field drops before its `SignalSubscriberDrop` (`write.rs:164`),
`update_subscribers` takes a fresh read (`signal.rs:260`), and `mark_dirty` on
a memo only sets a flag and sends on a channel (`memo.rs:53`). The rules still
hold, but for the reasons stated at each rule, not that one.

## Always use `try_read()` for reactive signal reads

```rust
// WRONG — panics if signal is being written
let rooms = ROOMS.read();

// RIGHT — returns Err instead of panicking
let Ok(rooms) = ROOMS.try_read() else { return; };
```

**IMPORTANT:** In Dioxus 0.7.x, `try_read()` does NOT register signal
subscriptions when it returns `Err` — `Signal::try_read_unchecked`
propagates the borrow error with `?` (`signal.rs:409`) before reaching
`reactive_context.subscribe(...)` (`:413`). And a memo/effect REBUILDS its
dependency set on every pass: `reset_and_run_in` is `clear_subscribers()`
then `run_in(f)` (`dioxus-core` `reactive_context.rs:195`). So a pass that
early-returns on `Err` ends subscribed only to what it actually read — and
if that is nothing, to nothing at all. It then never re-evaluates.

### Required: anchor + nudge for every fallible read (freenet/river#555)

The two mitigations previously listed here are **not sufficient**, and
relying on them is what produced #499 and #555 (#397 is the same *blank
render* family but a different cause — a missing loading state — so it is
not evidence for this one):

- `defer()`-ing mutations does not prevent contention. It was already used
  throughout when all three bugs happened.
- The "backup subscription from the non-try signal" only downgrades the
  failure from *permanent* to *stuck until that other signal changes*. For
  `message_groups` the backup was `CURRENT_ROOM`, so the conversation froze
  until the reader switched rooms or reloaded.

Use `crate::util::signal_guard` instead. Read the anchor FIRST, before any
fallible read, and nudge on every degraded branch:

```rust
let thing = use_memo(move || {
    crate::util::signal_guard::anchor();          // before any try_read
    let Ok(rooms) = ROOMS.try_read() else {
        crate::util::signal_guard::schedule_nudge();
        return None;
    };
    ...
});
```

This applies to **`use_effect` as well as `use_memo`** — `use_effect` uses
the same `reset_and_run_in` primitive (`dioxus-hooks` `use_effect.rs:34`),
so an effect whose only read is a contended `try_read` is dead for the
session. A memo/effect with a SECOND fallible read needs a nudge on that
branch too; the anchor keeps the memo alive but does not re-subscribe the
signal that failed.

Still read an infallible signal before the fallible one where you can. It
costs nothing and leaves a backup if the nudge channel is ever broken.

`signal_guard`'s pin test enforces the ordering for the memos it lists, but
it only scans `use_memo(` bodies in an explicit file list — it cannot see
effects, helper functions, or unlisted files. Do not treat a green pin as
proof your new site is covered.

What actually holds a borrow across a recompute is **not diagnosed**. The
explanation this file used to give (a write's Drop firing notifications that
synchronously poll memos) does not hold in 0.7.9: the borrow is released
before subscribers are marked, and `mark_dirty` on a memo only sets a flag
and sends on a channel. Contention is nonetheless observed, and one `Err` is
enough to latch, so the guard is required regardless.

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

1. **RefCell re-entrancy**: a mutation run from a handler or a polled
   future can be re-entered while an outer borrow of the same signal is
   still live — `mark_dirty` explicitly "can run user code"
   (`signal.rs:262`), and this repo has an observed instance
   (`process_rooms()` being driven during a write-guard drop, see
   `sync_info.rs::rooms_awaiting_subscription`). A re-entrant borrow is a
   panic here, not a soft failure. `setTimeout(0)` breaks the call stack so
   no borrows are active. (This is NOT the debunked "Drop notifies
   synchronously while holding the borrow" story — see the note at the top.)

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

## Bind `oninput` on every EDITABLE `value:` field

`value` on `input` / `textarea` / `select` is a **volatile** attribute in
dioxus-html (`elements.rs:1488,1571,1594`). dioxus-core re-writes volatile
attributes to the DOM on every re-render, even when the rendered string has
not changed:

```text
// dioxus-core-0.7.9/src/diff/node.rs:463
if volatile || attribute_changed { self.write_attribute(...) }
```

and the interpreter then assigns the value whenever the LIVE DOM value differs
from the VDOM value:

```text
// dioxus-interpreter-js-0.7.9/src/ts/set_attribute.ts:31-33
case "value": ... else if (node.value !== value) node.value = value;
```

`onchange` fires on blur, not per keystroke. So a field bound only through
`onchange` still holds the PRE-TYPING text for as long as the user is typing,
and the next re-render resets the control and discards the edit.

```rust
// WRONG — any re-render wipes what the user has typed so far
textarea { value: "{description}", onchange: save }

// RIGHT — the signal tracks the DOM, so the re-write is a no-op
textarea {
    value: "{description}",
    oninput: move |evt| description.set(evt.value().to_string()),
    onchange: save,
}
```

Nothing about the broken call site looks wrong, and reasoning about the
component in isolation will not reveal it: the trigger is whatever unrelated
signal that component happens to read. `RoomDescriptionField` read
`CURRENT_ROOM` and `ROOMS` in its render body, so every room-state write
re-rendered it, and the owner's half-typed room description vanished
(freenet/river#564). It no longer reads them there, so today the trigger is a
prop change instead. That is exactly why the handler is the fix rather than
removing the subscription: `oninput` makes a field correct under ANY
re-render, and the trigger is never local to the component. There is no single
timer behind that cadence either: the
recurring ROOMS writers are the ~60 s idle liveness probe whose GET reply
merges unconditionally, the ~21 s `ProcessRooms` loop while a room awaits
subscription, and every subscription `UpdateNotification`. Do not go looking
for one interval to blame.

**This is the documented exception to the `defer()` rule above, and it is
narrow.** A controlled input's bound LOCAL signal must be written
SYNCHRONOUSLY from `oninput`. Deferring it lags the DOM by a `setTimeout(0)`,
so a re-render landing in that window writes the pre-keystroke value back and
drops characters, which is the very bug this section exists to prevent.

The exception covers local `use_signal` handles only. They have no external
subscribers, which is why `util.rs`'s
`event_handlers_never_write_a_global_signal_without_defer` does not object to
them. It is NOT a blanket `oninput` exemption: that pin does scan `oninput:`
(both inline closures and named handlers) and still requires `defer()` around a
write to a GLOBAL signal such as `ROOMS` or `CURRENT_ROOM`. If you need a
keystroke to reach global state, track the local signal synchronously and
defer the global write. See also the note at `members.rs`'s import-token
`oninput`.

Display-only controls may bind `value:` with no `oninput`, but must declare
themselves with a literal `readonly: true` (or `readonly: "true"`). A
conditional `readonly: !is_owner` does NOT count: the field is editable for
somebody, and that somebody is exactly who loses their typing.
`type="checkbox"` and `type="radio"` are exempt too, for the opposite reason:
dioxus reports their `evt.value()` as `"true"`/`"false"`, never the DOM's
`"on"`, so tracking a `value:` binding there would CAUSE a rewrite every
render. Bind `checked:`, which is not volatile.

`components.rs::volatile_value_binding_audit` scans every `.rs` file under
`ui/src` for this and pins the exact set of bindings it finds, so a field that
drops out of the scan fails loudly rather than quietly losing coverage. Two
limits worth knowing: it proves `oninput` is present and not a no-op, but not
that the handler writes the signal the `value:` binding reads; and it says
nothing about the OTHER route to the same symptom, where a parent unmounts the
field (a contended `try_read` memo returning `None`) and the remount re-seeds
`use_signal`. That second route is the #499/#555 contention family and
`oninput` does not help there.

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
