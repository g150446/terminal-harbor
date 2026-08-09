# Repository Guidelines

## Project Structure & Module Organization

Terminal Harbor is an MIT-licensed fork of WezTerm. Most of the tree is
upstream WezTerm; the fork's own work is the workspace sidebar, agent activity,
the mobile bridge, and the restart model. Read
[`docs/clean-room.md`](docs/clean-room.md) before adding features: it defines
the independent-implementation boundary this project works under, and requires
recording source and license in the commit, plus updating `NOTICE`, when code
is ported from another project.

Harbor-specific code lives in:

| Path | Responsibility |
| --- | --- |
| `wezterm-gui/src/harbor_workspace.rs` | Workspace registry, persistence, directory resolution, agent and activity aggregation |
| `wezterm-gui/src/termwindow/harbor_sidebar.rs` | Sidebar row rendering, formatting, colors |
| `wezterm-gui/src/harbor_mobile.rs` | HTTP bridge on port 7780 for the companion app |
| `wezterm-gui/src/harbor_restart.rs`, `wezterm/src/harbor_restart.rs` | Restart request handling, compatibility checks, restart helper |
| `wezterm-gui/src/harbor_settings.rs` | Harbor settings persistence |
| `wezterm-mux-server-impl/src/sessionhandler.rs` | Per-pane push loop; relays pane state and user vars to attached clients |
| `assets/macos/Terminal Harbor.app` | macOS bundle template |
| `ci/deploy.sh` | Bundle assembly, signing, notarization |

The sibling Flutter app lives at `../terminal-harbor-mobile`, alongside this
repository, and has its own `AGENTS.md`. The HTTP contract is its
`openapi/harbor-mobile.yaml`. Keep that file, the Rust handlers in
`harbor_mobile.rs`, and the mobile client in agreement; the
`/v1/workspaces/{id}/key` handler and its `terminal_key_code()` mapping define
which terminal keys the API accepts.

## Process Model

Three processes run separately. This shapes almost every decision here.

| Process | Role |
| --- | --- |
| `wezterm-gui` | Window, menus, sidebar, rendering |
| `wezterm-mux-server` | Panes, PTYs, shells, agent child processes |
| `wezterm` | CLI and restart helper |

The GUI attaches to the persistent mux server as a client, so the panes it sees
are `ClientPane`s, not `LocalPane`s. Anything implemented only for local panes
returns its default for the GUI — `Pane::get_foreground_process_name` is the
known example, which is why the server relays `TH_PANE_PROCESS`. Before relying
on a `Pane` method in GUI code, confirm `ClientPane` implements it. Verify the
topology with `wezterm cli list-clients`; the GUI's own pid appears there.

## Build, Test, and Development Commands

Run the pre-build gate from the repository root:

```sh
git status --short --branch
cargo +stable check -p wezterm-gui -p wezterm -p wezterm-mux-server
cargo +stable test -p wezterm-gui-subcommands harbor_tests
cargo +stable test -p wezterm harbor_restart::tests
cargo +stable test -p wezterm-gui harbor_restart::tests
cargo +stable test -p wezterm-gui harbor
git diff --check
cargo +stable build --release
```

`make build`, `make check`, and `make test` cover the broader upstream suite.
Full detail, including bundle assembly and rollback, is in
[`docs/maintenance.md`](docs/maintenance.md).

## Coding Style & Naming Conventions

Format with `cargo +nightly fmt`; stable rustfmt ignores the nightly-only
settings in `.rustfmt.toml` and must not be used to judge formatting. Do not
mechanically reformat files this change did not touch, and do not mix upstream
or in-progress diffs into the same commit.

Comment why, not what, and match the density of the surrounding code. Name
constants that cross a process or crate boundary, and pin them with a test
rather than trusting two copies to stay equal.

## Testing Guidelines

Name tests by observable behavior. Cover parsing, formatting, fallback order,
and boundary conditions with unit tests next to the code. Width-sensitive UI
formatting must be tested with double-width CJK input, not only ASCII.

Unit tests cannot see the process model: they exercise pure functions, so they
pass whether or not the feature works against a `ClientPane`. When a change
depends on live pane state, verify it against the running app as well and say
which of the two you did.

## Deployment & Restart

Whenever a task changes the macOS application's code, resources, configuration,
or bundled contents, finish the task by building a complete release bundle and
replacing `/Applications/Terminal Harbor.app` with it. Do not treat a source
change or a successful local build as deployed. Stage, sign, verify, and swap
the whole bundle as described below, then use the restart mode required by the
change. Record the deployment in `docs/release-log.md`.

Never overwrite files inside the running bundle. Stage a complete bundle in a
temporary directory, sign it, verify the signature, and only then swap it into
`/Applications/Terminal Harbor.app`, keeping the previous bundle until rollback
is no longer needed.

Choose the restart mode by what changed, and state it in the report:

| Change | Restart |
| --- | --- |
| GUI, menus, rendering, sidebar only | `wezterm restart` |
| mux server, wire protocol, PTY, process inspection | `wezterm restart --full` |

A preserving restart re-executes the binary at the GUI's own path, so it picks
up a new bundle but leaves the running mux server untouched. Shipping a mux
server change with a preserving restart fails silently: the feature is simply
absent while the old host keeps running. See
[`docs/restarting.md`](docs/restarting.md).

Record every deployment in [`docs/release-log.md`](docs/release-log.md) with the
fields `docs/maintenance.md` lists. Note that `codesign` rewrites the binary, so
hashes of the installed bundle never match `target/release`.

## Documentation Maintenance

Whenever code is added or modified, review the related maintenance
documentation in the same change. Update it whenever the implementation would
otherwise make the documentation incomplete, inaccurate, or misleading. In
particular, keep `docs/`, `README.md`, and operational procedures aligned with
changes to:

- Internal or external APIs, user vars, data formats, and communication contracts.
- Architecture, component responsibilities, and the process model.
- Build, test, deployment, configuration, and restart procedures.
- Security, persistence, migration, recovery, and rollback behavior.
- User-visible behavior, constraints, and compatibility requirements.

`docs/harbor-sidebar.md` owns the sidebar row contract,
`docs/mobile-bridge.md` the HTTP bridge, `docs/restarting.md` the user-facing
restart rules, `docs/maintenance.md` the runbook, and `docs/release-log.md` the
deployment history. `docs/changelog.md` is upstream WezTerm's; do not put
Harbor notes there.

If no suitable document exists and future maintainers need context, add a
focused Markdown file under `docs/` that explains the reason for the change,
its assumptions and impact, and how to verify it, then link it from the
document that owns the area. Do not create documentation for trivial refactors
or self-evident fixes. When documentation does not need an update, state that
fact and the reason briefly in the completion report.

When reporting a commit or a push, report the documentation maintenance too:
name the documents updated in the same change and what they now say. A report
that lists only code and tests leaves the reader unable to tell whether this
section was applied or silently skipped.

## Commit & Pull Request Guidelines

Recent commits use short imperative subjects such as `Relay the foreground
process name to mux clients` or `Raise the screen mirror line cap to 20000`.
Bodies explain why the change was needed and what constraint shaped it, not
what the diff already shows. Keep each commit focused and include its tests and
documentation; split work so each commit builds on its own.

Pull requests should describe the user-visible change, the restart mode
required, verification commands, and rollback impact.

## Security & Operations

The sidebar and APIs deliberately expose basenames rather than full paths.
Never leak local paths, tokens, pairing URIs, or private topology into the
sidebar, logs, or commits. Preserve the existing redaction when adding text
that comes from pane contents.

Code that kills a process by PID file must confirm the target executable name
first; do not signal a process on the strength of a PID alone.
