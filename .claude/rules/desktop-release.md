---
description: Releasing the River desktop app (river-desktop-v* tags), its version floors, signing secrets and the release workflow's checks
globs:
  - apps/desktop/**
  - .github/workflows/release-desktop.yml
paths:
  - "apps/desktop/**"
  - "apps/Cargo.toml"
  - ".github/workflows/release-desktop.yml"
  - "scripts/tests/verify-release-test.sh"
---

# Releasing the desktop app

Releases are GitHub releases under `river-desktop-v<version>` tags, built and signed by `.github/workflows/release-desktop.yml`.

| Asset | Platform |
|---|---|
| Tarball | Linux x86_64 |
| `River.app`, zipped, signed and notarized | macOS universal |
| Zip, signed | Windows x86_64 |
| `SHA256SUMS.txt` | all |

The workflow creates the release as a **draft**. A maintainer publishes it.

## Version floors

| Constant (`apps/desktop/src/release.rs`) | Meaning | When to raise |
|---|---|---|
| `MIN_INSTALL_VERSION` | Oldest Freenet the installer installs | **Every desktop release**, to the current Freenet release. It is the only thing stopping someone who can edit GitHub releases (but not sign them) from steering new users to an old signed release |
| `MIN_NODE_VERSION` | Oldest running node River accepts | Only when River needs something newer. Nodes update over hours to days, so raising it early tells people with a working node that theirs is too old |

The tests derive from both constants, so raising either needs no test change. Reading rooms needs app grants (freenet-core v0.2.156), and `MIN_NODE_VERSION` deliberately stays below that: an older node passes setup and then shows a "Freenet needs an update" screen for the rooms, rather than failing setup for a node that otherwise works.

The installer ties a release's signed checksums to its tag by finding `Freenet-<version>.dmg` in them, on every platform (`check_manifest_version` in `release.rs`). If freenet-core stops shipping that DMG, River refuses every release until that check changes.

## One-time setup (repository admin, in this order)

1. Create the `release` environment.
2. Give it a deployment policy that allows only `river-desktop-v*` tags.
3. Add a tag ruleset that restricts creating `river-desktop-v*` tags to admins.
4. Only then add the signing secrets (below) to the environment.

The `if: github.event_name == 'push'` conditions on the sign and release jobs are not a security boundary. Every branch and tag runs its own copy of the workflow file, so whoever can push one can remove them. The environment's deployment policy, which GitHub enforces outside the file, keeps the signing secrets to `river-desktop-v*` tags, and the ruleset decides who can create those. Both sign jobs use the one `release` environment, so a required-reviewer rule on it asks for approval twice per release.

## Each release

1. In one PR, raise `MIN_INSTALL_VERSION` to the current Freenet release (check the latest freenet-core release; nothing automates this) and bump `[workspace.package] version` in `apps/Cargo.toml`. Merge it.
2. `git tag river-desktop-v<version> <main commit> && git push origin river-desktop-v<version>`
3. Download the draft's assets, check them against `SHA256SUMS.txt`, and run them on a real Mac and a real Windows PC: install Freenet, approve the grant, see rooms, send a message.
4. Publish the draft with "Set as the latest release" unticked: `gh release edit <tag> --draft=false --latest=false` (API: `make_latest=false`). The workflow cannot enforce this. Every desktop release, from 1.0 on too, stays off "Latest" because riverctl releases hold the repo's "Latest" badge. 0.x versions are prereleases.

To rehearse, run the workflow by hand (`workflow_dispatch`) on any branch. This dry run runs every `verify` check except "the commit is on main", builds, packages and writes the notes header. It does not exercise the secrets check, signing, or the draft-release logic. Those have no rehearsal path other than a real tag, and the draft is what makes a first run safe.

## What `verify` refuses

The `verify` job runs `apps/desktop/packaging/verify-release.sh`, tested by `scripts/tests/verify-release-test.sh` in `build.yml`. It refuses the tag unless:

| Check | Why |
|---|---|
| Tag version matches `apps/Cargo.toml` | |
| The commit is on main | |
| `ui/public/contracts/` is byte-identical to the latest `chore(publish): bump webapp contract version` commit on main's first-parent history | The app compiles in the chat delegate and room contract WASM and reads and writes the user's rooms under their keys, so it must carry exactly what the live web UI carries (it carries rooms forward from EARLIER keys, never to newer ones) |
| Both WASMs' BLAKE3 hashes match `pointer-records.toml` | Same |

If the WASM changed since the last UI publish, publish the UI first. The release notes record the published web UI version and both hashes, and list what the app does not do yet (direct messages, moderation, nickname edits, room upgrades).

## Re-keying publishes

A re-keying River publish needs a new desktop release ready first. Once its pointer records are published, every installed desktop build is read-only until the user updates (`.claude/rules/river-publish.md`, step 7). The new release carries a desktop-only user's rooms from the earlier chat delegates and room-contract keys the registries list (`apps/river-app/src/rooms/carry.rs` and `rekey.rs`, freenet/river#889), so the registry rows the publish adds are all it needs. **Before tagging, run the rehearsal** on the release commit: `apps/scripts/rekey-rehearsal.sh <work dir>` (with no commit it seeds from the build before the last chat-delegate change; give older commits too, oldest first, to rehearse a skipped release). It must end with `rekey rehearsal passed`; record the command and result in the release PR.

## What CI checks per platform

| Platform | Version check |
|---|---|
| Linux | `river --version` runs |
| macOS arm64 slice | `river --version` runs |
| macOS x86_64 slice | Compiled and linked, never executed |
| Windows | The exe's `ProductVersion` resource. `river.exe --version` prints nothing: release builds use the GUI subsystem, so there is no console |

## Packaging and signing

Packaging lives in `apps/desktop/packaging/`. The icons in `icons/` are generated from `ui/assets/river_logo.svg` and committed. Whoever changes the logo runs `generate-icons.sh` and commits the result; `apps.yml` fails a PR that changes the logo without the icons. `apps/desktop/build.rs` embeds the Windows icon and version resource.

Pull requests that touch the packaging run the build and package jobs on all three platforms, without secrets. Signing runs only for tags, in jobs that use the `release` environment. It holds:

| Secret | For |
|---|---|
| `APPLE_DEVELOPER_ID_P12_BASE64`, `APPLE_DEVELOPER_ID_P12_PASSWORD` | macOS code signing |
| `APPLE_ASC_API_KEY_P8_BASE64`, `APPLE_ASC_API_KEY_ID`, `APPLE_ASC_API_KEY_ISSUER_ID` | Notarization |
| `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID` | Windows signing. The Entra federated credential is for `repo:freenet/river:environment:release` |

`SHA256SUMS.txt` is not signed. **Never use `FREENET_RELEASE_SIGNING_KEY` here**: it is the Freenet updater's trust root.
