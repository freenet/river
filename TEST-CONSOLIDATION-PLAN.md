# Test consolidation and geometry-helper cleanup

## Objective and scope

Consolidate overlapping history-scroll tests and centralize repeated browser-test geometry reads without changing River’s scrolling behavior, timing policy, or supported browser coverage.

This plan is based on `feat/history-scroll-anchor` at `98111672b87ea8ea5c9cbb400ac79fff6ef297c1`. The earlier audit identified 45 tests in the new history-scroll module, plus extensive browser coverage of anchoring, gesture settlement, animation, mobile reveal, and windowing. Check the current branch before implementing; line numbers and test counts may have moved.

Deliverables:

- Three redundant Rust scenarios consolidated into retained tests with their distinctive assertions preserved.
- A native-settlement matrix with explicit expected outcomes instead of an expectation copied from the implementation.
- One strong parameterized parked-resize test covering both top-of-history and mid-history starts.
- One focused, test-only geometry module used by the existing specs and timing fixtures.
- A before/after coverage inventory and evidence that retained regressions still detect representative faults.

Out of scope:

- Production scroll behavior, animation tuning, completion-notification optimization, or nickname/mention optimizations.
- Reducing browser projects, fractional-scale coverage, native-input probes, diagnostic sweeps, or event-order permutations.
- Removing the single-arrival animation case: its scheduling differs from the speed/completion tests, so it needs a separate evidence-backed decision.
- Rewriting the pending-scroll, trim, or opening-snap source pins.
- Splitting entire fixture files, moving historical runtime documentation, adding dependencies, publishing, or opening a PR automatically.

## File structure — lock this in before defining implementation tasks

Follow the existing test layout. Add one small file for geometry; keep timing and instrumentation in their current focused fixtures. Do not extract Rust tests into another file solely to reduce file size.

| File | Action | Responsibility after the change |
|---|---|---|
| `ui/src/components/conversation/history_scroll.rs` | Modify its `#[cfg(test)] mod tests` only | Pure scroll-policy and state-transition tests. Production items above the test module remain unchanged in the final diff. |
| `ui/tests/history-scroll-geometry.ts` | Create | Shared `RowPosition` type, self-contained in-page geometry installation, newest-visible-message selection, saved-row position reads, and small Node-side readers/drift functions. No timers, event interception, clock setup, state writes, or scroll policy. |
| `ui/tests/conversation-autoscroll.spec.ts` | Modify | Keep behavioral scenarios, arrival instrumentation, and settlement controls. Import shared geometry; replace the weak parked-resize case with stronger top/mid variants. |
| `ui/tests/history-follow-fixture.ts` | Modify | Keep frame stepping, recorder lifecycle, correction-order controls, logs, and deadline observation. Move generic geometry/type exports to the new module and update internal callers. |
| `ui/tests/history-scroll-fixture.ts` | Modify | Keep seek-clock, gated-settlement, and debounce sections. Delegate their repeated message geometry to the shared primitive without changing waits, event ordering, or timer adapters. |
| `ui/tests/history-clamp-fixture.ts` | Modify | Keep overflow setup, clamp premises, and cleanup. Move generic saved-visible-row drift reading to the geometry module. |
| `ui/tests/conversation-follow-state.spec.ts` | Modify | Register geometry before navigation, update imports, and replace duplicate geometry scans inside native recorder callbacks. Keep all cases and timeline semantics. |
| `ui/tests/conversation-scroll-debounce.spec.ts` | Modify | Update imports for shared geometry/drift types. Fallback installation remains owned by `debounceUseFallback`. Keep every fallback case. |
| `ui/tests/conversation-seek-takeover-diagnostic.spec.ts` | Modify | Update geometry imports and setup. Retain opt-in discovery, conditional diagnostic assertions, native-clock waits, and logging. |
| `TEST-CONSOLIDATION-PLAN.md` | Update during implementation | Record completed steps, baseline problems, retained coverage, fault checks, and any justified deviations. |

Related files to read, but not modify for this scope:

- `AGENTS.md`: static-build browser testing, verified build identity, free-port/base-URL rules, stable test IDs, and pre-PR workspace checks.
- `.claude/rules/dioxus-signal-safety.md`: required before editing under `ui/src/`, even when the final change is confined to a test module.
- `ui/src/components/conversation.rs`: `WindowAnchor`, the parked/follow behavior, and the wiring pins near `autoscroll_wiring_pins`. These explain why some apparent overlap must remain.
- `ui/tests/history-scroll-helpers.ts`: existing shared running-clock setup and clock-independent scroll-height/offset reads. Reuse them; do not create competing copies in the geometry module.
- `ui/tests/history-missing-anchor-fixture.ts`: selects whole anchor neighborhoods, including non-message rows. Its richer selection is distinct from newest-visible-message geometry and stays intact.
- `ui/tests/conversation-seek-speed.spec.ts`: retained animation speed and native-clock completion coverage; no edits expected.
- `ui/tests/playwright.config.ts`: project matrix, fractional geometry tags, diagnostic opt-in, and environment-provided base URL. No changes expected.
- `Makefile.toml`: build/test tasks. Its `test-ui-playwright` description still mentions `dx serve`; follow `AGENTS.md` and the config’s static-server instructions instead.

## Coverage contracts and decomposition decisions

### Rust consolidation map

| Consolidate/remove after strengthening retained coverage | Retained coverage | Required detail |
|---|---|---|
| `a_constrained_restore_preserves_intent_until_the_next_reader_move` | `missing_or_constrained_restores_preserve_a_gesture` | Preserve the extra assertion that a pending layout still selects `GesturePosition::Preserve` after a reader move cleared the earlier latch. Keep missing, fully constrained, and partially constrained variants; later success must not clear the latch. |
| `an_intermediate_clamp_is_layout_only_within_the_allowance` | `the_allowance_boundary_is_layout` and `any_move_with_an_unchanged_layout_and_a_reachable_offset_is_the_reader` | Retain a reachable offset below the final end, movement within the allowance, movement one pixel beyond it, unchanged-layout reader movement, and the documented intermediate-clamp residual. |
| `a_new_rooms_empty_anchor_is_missing_and_its_forced_snap_still_owed` | `a_room_switch_forgets_the_old_rooms_position`, `a_restore_distinguishes_a_missing_anchor_from_one_at_its_gap`, and `a_missing_anchor_with_no_gesture_preserves_nothing` | Keep reset clearing the anchor/gesture/latch, force and pin remaining true, empty anchors returning missing, and a subsequent missing restore not creating a preservation latch. Put the post-reset sequence in the room-reset test if desired. |

Keep `native_settle_policy_matrix`, but replace its derived `expected` expression with explicit rows. For a view top of 940:

| Pending cause | No correction | Correction at 940 | Correction at another top |
|---|---|---|---|
| None | Settle | Refuse | Settle |
| Echo | Settle | Refuse | Settle |
| Reader | Settle | Refuse | Settle |
| Layout | Refuse | Refuse | Refuse |

Do not compute these expected booleans using the expression under test. Keep neighboring stateful tests: clearing correction evidence, carrying it through layout, and refusing a correction’s own end are more than pure truth-table checks.

### Browser resize consolidation

Combine these existing requirements in `conversation-autoscroll.spec.ts`:

- `does not drag a parked reader down when a resize reflows the history` currently starts at `scrollTop = 0` and only checks that the reader remains outside the follow band.
- `the same message is still in place after resizing there and back @fractional-geometry` starts mid-history and checks message identity/gap on both resize legs.

Make the stronger case parameterized over `top-of-history` and `mid-history`. Preserve a visible-message snapshot, the unpinned premise, the same-message/gap checks on narrowing and widening, and explicit post-resize distance outside the follow band. Remove the weak standalone case only after both variants pass and a broken restore is detected. Keep the fractional-geometry tag on both variants.

If the top-of-history case encounters an unreachable saved gap, document and assert the existing constrained-restore behavior rather than silently relaxing tolerances or changing production code. Retain a separate meaningful boundary case if it cannot share the strong test’s assertions cleanly.

### Shared geometry design

Use a small test-only namespace such as `window.__riverHistoryGeometry`. Its installer must be self-contained: Playwright serializes callbacks into the page, so a browser callback cannot close over imported Node-side functions.

The in-page API should contain only:

- `newestVisible(container)`: newest message intersecting the viewport, returning `{ id, gap }` or `null`.
- `positionOf(container, id)`: saved row identity, bottom-relative gap, and visibility information, returning `null` for missing/out-of-container rows.

The module can expose thin Node-side readers with descriptive names for the existing three semantics:

1. **Newest-message drift:** the saved row must still be the newest visible message; missing, replaced, or no longer newest yields `Infinity`.
2. **Saved-row drift:** compare the saved row’s gap without requiring it to remain newest; retain the autoscroll fallback case’s existing visibility semantics.
3. **Saved-visible-row drift:** the saved row must remain visible, but a newer row may enter the viewport; retain the clamp tests’ existing behavior.

Do not silently collapse these into one assertion. In particular, `clampRowDrift` is intentionally different from `rowDrift`.

Installation and independence requirements:

- Register geometry before every relevant first navigation; install it into the current document as needed for fixture entry points that can run after navigation.
- Make installation idempotent and fail clearly if a required setup path is missing.
- Do not depend on relative execution order between separate `addInitScript` registrations. Other installers should resolve the namespace when a geometry method is called, not during their own initialization.
- Use `[id^="msg-"]` for independent message selection, preserving current tests’ separation from the production `anchor-row` markers. Leave tests of all anchor row types separate.
- Do not call production `HistoryScroll` helpers, read its internal state, change geometry, or reproduce its follow policy.
- Do not change the paused-clock timer-ID mapping, `scrollend` gates, frame waits, timestamp sampling, observer ordering, or cleanup behavior.
- Keep drift measurements fresh and synchronous within browser callbacks when a scenario requires one-task evidence. Do not introduce extra asynchronous reads into those callbacks.

## Implementation tasks — each numbered step is one action

### Phase A: establish a trustworthy baseline

1. Read the current branch state and applicable instructions.
2. Record default Playwright discovery by file/project in an external temporary evidence directory.
3. Record opt-in diagnostic discovery in that evidence directory.
4. Run the native history-scroll tests and save their baseline result.
5. Verify the installed Dioxus CLI matches the resolved Dioxus version.
6. Build the existing example-data/no-sync release UI.
7. Start a static server for that artifact on a verified free port.
8. Confirm the browser-loaded artifact identity and anchor-row markers.
9. Run the four default scroll specs across the existing project matrix with retries disabled.
10. Record any baseline failures before proceeding with test deletions.

Do not attribute pre-existing failures to cleanup. Resolve environmental problems without changing unrelated source; retain affected tests if deletion safety cannot be established.

### Phase B: consolidate pure Rust cases

11. Add the missing pending-layout assertion to `missing_or_constrained_restores_preserve_a_gesture`.
12. Run the retained preservation test.
13. Remove `a_constrained_restore_preserves_intent_until_the_next_reader_move`.
14. Add any uncovered intermediate-clamp example and residual comment to the retained classification cases.
15. Run the retained classification tests.
16. Remove `an_intermediate_clamp_is_layout_only_within_the_allowance`.
17. Add the post-reset missing-restore sequence to the room-reset test.
18. Run the retained room-reset and missing-anchor tests.
19. Remove `a_new_rooms_empty_anchor_is_missing_and_its_forced_snap_still_owed`.
20. Rewrite `native_settle_policy_matrix` with the explicit expected rows above.
21. Run the complete native history-scroll test module.
22. Fault-check the retained Rust coverage using temporary targeted mutations.
23. Restore every temporary mutation to the production implementation.
24. Run the native history-scroll test module against the restored implementation.
25. Commit only the Rust test consolidation.

For this refactor, retained tests should normally be green before code cleanup. Do not invent a failing feature test just to follow a red/green ritual; deliberate fault checks supply the negative evidence.

### Phase C: extract geometry and migrate its callers

26. Create `ui/tests/history-scroll-geometry.ts` with the focused API and setup contract above.
27. Migrate the follow fixture’s type/geometry exports and its follow-state spec callers.
28. Run the follow-state spec on Chromium with retries disabled.
29. Migrate autoscroll’s in-page geometry and Node-side drift reads.
30. Run the autoscroll spec on Chromium with retries disabled.
31. Migrate gated-settlement geometry in `history-scroll-fixture.ts`.
32. Run autoscroll’s controlled gesture cases with retries disabled.
33. Migrate debounce geometry through `debounceUseFallback` and its existing fixture API.
34. Run the debounce spec on Chromium with retries disabled.
35. Migrate saved-visible-row drift from the clamp fixture and its two consuming specs.
36. Run the clamp and missing-anchor cases in the follow-state/debounce specs.
37. Migrate repeated inline message scans in the follow-state native recorder callbacks.
38. Migrate the opt-in takeover diagnostic’s geometry imports/setup.
39. Run opt-in diagnostic discovery to verify imports and registration remain valid.
40. Remove obsolete geometry exports, wrappers, and duplicate local types.
41. Search for remaining duplicated newest-visible-message scans in the touched files.
42. Run all four default scroll specs across the existing project matrix with retries disabled.
43. Commit only the geometry-helper extraction and caller migration.

Keep compatibility adapters only where timing/recorder ownership requires them, such as `window.__riverDebounce.newestVisible()`. Each adapter must delegate to the shared read rather than retaining another DOM scan. Do not preserve unused re-exports merely to avoid updating imports.

### Phase D: strengthen and consolidate resize behavior

44. Parameterize the strong resize test over top-of-history and mid-history starts.
45. Add the explicit unpinned checks to both variants.
46. Run both strong resize variants across the existing engines and fractional-scale project.
47. Fault-check both variants against a temporary broken anchor restore.
48. Restore the temporary production mutation.
49. Remove the weaker standalone parked-resize test.
50. Run both retained resize variants against the restored implementation.
51. Commit only the resize-test consolidation.

### Phase E: final verification and handoff

52. Compare default and diagnostic discovery against the baseline inventory.
53. Check formatting without rewriting unrelated files.
54. Run all native UI binary tests.
55. Run the full default Playwright suite across the existing matrix with retries disabled.
56. Run the opt-in takeover diagnostic separately to check migrated setup/callers.
57. Run the required workspace tests before any PR.
58. Inspect the final diff for production-code, configuration, dependency, and unrelated-file changes.
59. Record results and deviations in this plan.
60. Commit the completed verification record.

Stage explicit intended paths only. The repository currently has unrelated untracked `.claude/skills/paral-pr-rev/` and `pr-732-parallel-review.md`; preserve them and exclude them from cleanup commits.

## Verification commands and evidence

Run commands from the repository root unless noted. Choose and record a free port; `8093` below is an example, not a reservation. Do not reuse an existing server from another worktree.

```sh
git status --short
git rev-parse HEAD
cargo tree -p dioxus
dx --version
cargo test -p river-ui --bins history_scroll::tests
cargo make build-ui-example-no-sync
python3 -m http.server 8093 --bind 127.0.0.1 \
  --directory target/dx/river-ui/release/web/public
```

Use the compatible CLI, potentially `~/.cargo/bin/dx`, if an older binary is earlier on PATH. Match the resolved dependency rather than the declared `ui/Cargo.toml` version. Keep the static server in its own process; shut down only the process started for this implementation when testing finishes.

From `ui/tests/`:

```sh
npx playwright test --list --reporter=json
RIVER_SCROLL_DIAGNOSTICS=1 npx playwright test --list --reporter=json

PLAYWRIGHT_BASE_URL=http://127.0.0.1:8093 npx playwright test \
  conversation-autoscroll.spec.ts \
  conversation-follow-state.spec.ts \
  conversation-scroll-debounce.spec.ts \
  conversation-seek-speed.spec.ts --retries=0

PLAYWRIGHT_BASE_URL=http://127.0.0.1:8093 npx playwright test \
  conversation-autoscroll.spec.ts --grep 'resizing there and back' --retries=0

PLAYWRIGHT_BASE_URL=http://127.0.0.1:8093 npx playwright test --retries=0

RIVER_SCROLL_DIAGNOSTICS=1 PLAYWRIGHT_BASE_URL=http://127.0.0.1:8093 \
  npx playwright test conversation-seek-takeover-diagnostic.spec.ts --retries=0
```

Retain an identifying phrase in the parameterized resize titles or adjust the targeted command accordingly. Add `--project=chromium` for the explicitly scoped intermediate checks; omit it for the final matrix. Record runtime skips separately from failures. A diagnostic pass remains observational when its conditional outcome is not reached.

Final native/workspace checks:

```sh
cargo fmt --all -- --check
cargo test -p river-ui --bins
cargo make test
```

`cargo make test` includes tasks targeting `x86_64-unknown-linux-gnu`; on a host without a usable target/toolchain, report the limitation and obtain that check in the existing supported environment. Do not silently substitute native UI tests for the required workspace check, or modify test configuration to force a green result.

No standalone TypeScript typecheck script or tsconfig is currently present under `ui/tests`. Do not invent a new toolchain for this cleanup. Discovery catches load/import problems; browser executions exercise serialized callbacks and namespace setup. Use any existing CI typecheck if one becomes available during implementation.

### Targeted fault checks

Perform one temporary fault at a time in a clean tracked state and retain evidence that the intended test fails. Restore only the exact temporary edit, preserving other work. Never commit a fault or publish its build.

| Temporary fault | Retained coverage expected to fail |
|---|---|
| Allow a later successful restore to clear `preserve_gesture_position` | `missing_or_constrained_restores_preserve_a_gesture` |
| Fail to clear the preservation latch on reader movement | Retained preservation and missing/no-gesture boundary coverage |
| Change the layout allowance comparison to exclude its boundary | `the_allowance_boundary_is_layout` |
| Fail to clear a room’s anchor/preservation state during reset | `a_room_switch_forgets_the_old_rooms_position` |
| Accept a native end that matches correction evidence or pending layout | Explicit settlement matrix and correction-end cases |
| Skip the parked reader’s anchor restoration | Both strong resize variants and relevant deletion/late-content cases |

For browser faults, rebuild the static artifact and confirm the browser loaded the mutated build before believing a failure or pass. Rebuild and verify the restored artifact before normal regression testing. Do not use `dx serve` for these checks. If a proposed retained test does not detect its assigned fault, strengthen coverage or keep the overlapping test instead of deleting it.

## Acceptance criteria

- The final production implementation is unchanged; edits to `history_scroll.rs` are confined to its test module.
- All meaningful assertions from the three consolidated Rust tests survive in named retained cases.
- The settlement matrix’s expectations are literal outcomes, independent of the production predicate.
- Top-of-history and mid-history parked readers have strong resize coverage; no tolerances or setup premises were weakened to obtain a pass.
- Shared message selection/gap reading has one implementation across the targeted test fixtures; recorder-specific state, all-anchor neighborhood selection, and clock controls remain separate.
- Missing/out-of-container rows never report zero drift, and newest-row versus saved-row versus saved-visible-row semantics remain distinct.
- Geometry setup works after fresh navigation, room changes, and in paused/native-clock callers without relying on initializer ordering.
- Existing browser projects, skip policies, fractional tags, default diagnostic exclusion, timer-ID adapters, and event-order cases remain intact.
- Default discovery changes only for the documented resize consolidation; opt-in discovery retains the diagnostic. Three Rust test names disappear, with no loss of their substantive coverage.
- Baseline/final results and successful fault checks are recorded; pre-existing failures and unavailable required checks are disclosed.
- No unrelated review files, generated artifacts, migration/pointer records, Cargo files, publishing counters, or production hooks are included in the commits.

## Implementation record

- Status: implemented on `feat/history-scroll-anchor`. Test-only commits `e29e233b`, `11aceeb6`, `f59f3534`, plus this record. Production `history_scroll.rs` is unchanged; every hunk in that file is inside `mod tests`. No config, Cargo, migration, or pointer files changed.
- Baseline revision: `98111672b87ea8ea5c9cbb400ac79fff6ef297c1`. `dx` 0.7.9 matches resolved `dioxus` 0.7.9.
- Evidence directory: `/tmp/river-test-consolidation-evidence`. Static server: `127.0.0.1:8093`, serving `target/dx/river-ui/release/web/public`.
- Baseline native `history_scroll::tests`: 45 passed. Default discovery: 980 tests. Diagnostic discovery (`RIVER_SCROLL_DIAGNOSTICS=1`): 985, the extra five being `conversation-seek-takeover-diagnostic.spec.ts` on the five engine projects. Loaded artifact: `Built: 2026-10-02T11:12:22Z | Commit: 98111672`, with 14 `data-anchor-row` markers and 13 `msg-` rows in Team Chat Room. Four default scroll specs: 447 passed, 7 skipped, exit 0. No baseline failures.
- Rust consolidation. Removed `a_constrained_restore_preserves_intent_until_the_next_reader_move`, `an_intermediate_clamp_is_layout_only_within_the_allowance`, and `a_new_rooms_empty_anchor_is_missing_and_its_forced_snap_still_owed`. Their assertions now live on `missing_or_constrained_restores_preserve_a_gesture` (including `gesture_position(true) == Preserve` after the reader move), `the_allowance_boundary_is_layout` (reachable offset below the final end, and the intermediate-clamp residual), and `a_room_switch_forgets_the_old_rooms_position` (empty anchor is missing, and a following missing restore does not latch). `native_settle_policy_matrix` lists the twelve outcomes for view top 940. Module result after removal: 42 passed.
- Rust fault checks, one production edit at a time, then restored (`cmp` against the pre-fault file). A later successful restore that clears `preserve_gesture_position` fails `missing_or_constrained_restores_preserve_a_gesture`. `note_reader_move` leaving the latch set fails that same test. An allowance comparison of `<` instead of `<=` fails `the_allowance_boundary_is_layout`. `reset_for_room` leaving the anchor and the latch fails `a_room_switch_forgets_the_old_rooms_position` and also `replacing_an_interaction_forgets_a_missing_or_constrained_restore`. `native_end_settles` returning true fails the matrix plus `a_native_end_at_our_correction_does_not_settle_the_gesture`, `a_container_height_change_alone_keeps_our_corrections_end_refused`, `a_growth_clamps_own_end_is_refused_with_no_correction_evidence`, `a_growth_clamp_carries_an_existing_corrections_end_refusal`, `a_delivered_layout_keeps_correction_evidence_at_its_recorded_top`, and `a_reader_move_after_a_correction_lets_its_native_end_settle`.
- Geometry. `ui/tests/history-scroll-geometry.ts` owns `newestVisible` / `positionOf` and the three drifts: newest-message, saved-row, and saved-visible-row. Callers resolve the namespace when they read. Chromium, retries off: follow-state 25 passed, autoscroll 53 passed, debounce 9 passed. Four scroll specs across the matrix: 447 passed, 7 skipped. Diagnostic `--list` still shows the five opt-in tests.
- Resize. `the same message is still in place after resizing there and back from the top-of-history` and `... from the mid-history` both keep the message, the gap, and a distance outside the follow band, and both carry `@fractional-geometry`. The top start did not hit an unreachable gap, so it shares those assertions. Skipping `set_scroll_top` inside `restore_anchor` (artifact `Built: 2026-10-02T11:39:03Z | Commit: 11aceeb6`) failed all 12 variant runs and, on Chromium, the three removed/late-content cases. Restored artifact `Built: 2026-10-02T11:40:49Z | Commit: 11aceeb6`; the 12 variant runs passed. The weak `does not drag a parked reader down...` test was removed only after that.
- Discovery after the resize commit: default 980 → 981, diagnostic 985 → 986. The only title changes are in `conversation-autoscroll.spec.ts`: the weak test is gone from the five engine projects (it was not in `chromium-dpr-1.5`), and the old mid-history title is replaced by the top and mid titles, which adds the top variant to the fractional project. The diagnostic spec stays opt-in.
- Final checks, against the restored artifact above (the resize commit is test-only and does not change the wasm). `cargo fmt --all -- --check` passed. `cargo test -p river-ui --bins`: 1066 passed. Full default Playwright suite, retries off: 946 passed, 35 skipped, exit 0. Opt-in diagnostic: 4 passed, 1 skipped (mobile WebKit, as that spec skips), exit 0; a pass there is observational when the mid-flight order is not reached. `cargo make test` failed immediately: `x86_64-unknown-linux-gnu` is not installed (`E0463`, can't find `core`). This host only has `aarch64-apple-darwin`. That workspace check was not replaced with the native UI run, and the target was not installed to force a pass.
- Deviation. The "fail to clear the preservation latch on reader movement" fault does not fail `a_missing_anchor_with_no_gesture_preserves_nothing`. That test never sets the latch and never calls `note_reader_move`. The deleted test's reader-move assertions, including pending layout still selecting `Preserve`, are on `missing_or_constrained_restores_preserve_a_gesture`, which did fail. The overlapping test stayed deleted.
- Left untracked and out of these commits: `.claude/skills/paral-pr-rev/` and `pr-732-parallel-review.md`.
