# Fix PR #732’s two important scroll findings

## Goal
Fix Should Fix findings 1 and 2 in PR-732-review.md against head 63d23199: preserve parked reader intent through delivered/hidden layout clamps, and refuse correction ends across container-growth clamps.

## File map
- ui/src/components/conversation/history_scroll.rs: restore outcomes, gesture preservation, correction geometry, native tests, behavior docs.
- ui/tests/history-clamp-fixture.ts: clamp geometry and delivery observations.
- ui/tests/history-follow-fixture.ts: resize-before-end geometry and ordering.
- ui/tests/conversation-follow-state.spec.ts: native-end, hidden-reveal, and growth-clamp regressions.
- ui/tests/conversation-scroll-debounce.spec.ts: delivered-clamp deadline regression.
- ui/src/components/conversation.rs: jump-button observer documentation.

## Steps
1. Add delivered-clamp native and quiet-deadline regressions; retain settle-before-scroll cases.
2. Add mobile hidden-clamp reveal tests before and after the quiet deadline.
3. Build fresh static example-data/no-sync assets and prove these fail against the reviewed head at visible row preservation.
4. Return moved/constrained restore outcomes and latch constrained gesture restores until a reader move or interaction completion.
5. Use preserved gesture intent in normal and reveal settlement; cover lifecycle and restoration outcomes with native tests.
6. Verify the first fix through tall inbound arrivals, preserving the saved row gap within 4px and leaving parked readers outside the 100px follow band.
7. Add and prove the correction → container growth → end → observer regression with an in-band held reader.
8. Rebase existing correction evidence through known layout geometry, projecting pending clamps before native end judgment and rebasing delivered layout before recording new corrections.
9. Cover both growth geometries, reader revisions, quiet settlement, and absent corrections with native tests.
10. Correct related behavior comments without expanding scope to the review’s other suggestions.
11. Run native tests with and without example-data/no-sync, formatting, clippy, workspace tests, affected Playwright specs, and the full browser suite.
12. Inspect the diff and commit with Conventional Commit messages and test evidence.

## Validation
Check dx against resolved Dioxus (currently 0.7.9). Build with cargo make build-ui-example-no-sync and serve target/dx/river-ui/release/web/public on a free port with PLAYWRIGHT_BASE_URL. Require event-order/geometry premises and reproduce failures before fixes. Preserve normal in-band following, legitimate reader settlement, existing height-only resize refusal, seek takeover, and hidden deadline cancellation.

## Boundaries
UI-only changes. No public API, contract-state, dependencies, migration, pointer-record, or publishing changes. Read AGENTS.md and .claude/rules/dioxus-signal-safety.md before editing.

## Implementation adjustments
- Treat a pending hidden gesture’s reveal scroll as layout before capture. WebKit can deliver it before the observer and short of the final scroll range.
- Allow nested reveal work a 1ms clock tick, with an explicit assertion that the before-deadline case still completes before the original deadline. Keep the existing arrival fixture unchanged.

## Validation evidence
- Fresh static baseline: all four new delivered/hidden clamp cases failed at saved-row preservation (399px drift or the row leaving view).
- After the first fix: six native/deadline/reveal clamp cases passed; the separate growth case failed at 20px row drift.
- Final UI native tests: 1,063 passed; example-data/no-sync: 1,068 passed.
- cargo fmt --check and git diff --check passed.
- cargo make test targets x86_64-unknown-linux-gnu, which is not installed on this Mac. Equivalent host-platform tests for its six packages passed, including web-container integration tests (269 tests total).
- cargo make clippy is blocked by pre-existing common/ lint errors (doc indentation, a constant assertion, and repeat(1)); scoped UI Clippy completed with existing warnings and no diagnostics in history_scroll.rs.
- Full Playwright suite: 922 passed, 36 existing skips, and two Firefox retries (the new reveal fixture and an existing join-arrival fixture). The reveal scheduling adjustment was verified separately with 10/10 Firefox repetitions.
- Final controlled clamp/growth cases across all five browser projects: 35/35 passed without retries after the reveal fixture adjustment.
