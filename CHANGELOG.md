# Changelog

All notable changes to this project are documented here.
The format follows Keep a Changelog and Semantic Versioning.

## [Unreleased]

## [0.1.9] - 2026-07-30

### Changed
- GitHub synchronization is several times faster. The five task searches and the
  per-repository workflow-run lookups now run as one concurrent batch instead of
  one after another, the run lookups request only the four fields the CI-link
  matcher reads, their results are reused between polls while the snapshot still
  covers the notification, and the account login is read once per process.
  Measured on a 50-notification inbox: notifications 43.6 s to 6.0 s, tasks 9.9 s
  to 2.4 s, with byte-identical output.
- Counting unread notifications no longer resolves CI links (up to one Actions
  call per repository, for links nothing displays), so marking them read costs a
  single request.
- Each `gh` invocation receives the token in its environment instead of reading
  the keyring itself; the token is read once per process. Concurrent keyring
  reads serialize on the Secret Service, which showed as 5.7 s against 2.1 s over
  fifteen parallel calls.
- The dashboard applies tasks and notifications as each arrives instead of after
  both, and a refresh triggered by the window regaining focus is skipped when one
  has just finished, so ordinary window switching no longer restarts a full
  synchronization every time.
- The reload that follows a read-state write refreshes only the notification
  column instead of the whole server.

### Fixed
- Marking all notifications read no longer shows two spinners at once: the button
  reports the write itself, and the reload that follows is reported only by the
  sync bar (the button stays disabled meanwhile).

## [0.1.8] - 2026-07-21

### Added
- Settings now has an About section showing the running version, with a link to
  the project homepage; the version also appears on the up-to-date indicator's
  hover.
- Debug devtools in the GUI are opt-in via the `LABA_DEVTOOLS` environment
  variable (off by default), so a normal launch does not expose them.
- Project landing page published on GitHub Pages, with the screenshot openable
  in a full-screen lightbox.

### Fixed
- Demo dashboard: replaced a domain-specific sample field with a neutral
  fictional "Rank" so the mock data carries no real-world terms.

## [0.1.7] - 2026-07-18

### Added
- Always-visible update indicator in the header, reflecting the launch update
  check in every state: checking, an available version (click to open the update
  banner), up to date, or check failed (click to retry). It stays visible after
  the update banner is dismissed, so the update action is never lost.
- Setting to turn off checking for updates on launch (on by default). When off,
  the app does not contact the release server and the header indicator is hidden.

## [0.1.6] - 2026-07-18

### Added
- Settings: manage servers from the GUI — remove a server profile (and its
  stored token), set the default server, and sign out of an OpenProject server
  without removing it. When signing out is not applicable the row explains why:
  GitHub servers authenticate through the `gh` CLI (no token is stored here), and
  an OpenProject server with no token is simply not signed in.
- GitHub CI notifications (check-suite results) now link to the repository's
  Actions page, so a "CI run failed" notification is clickable like issue and
  pull-request notifications instead of staying plain text.
- Notifications now show each item's date and time, rendered in the configured
  timezone. A new setting switches the display to a relative label ("5 minutes
  ago") instead; either way the other form is available on hover.
- CI notifications are tinted by the run outcome: a failed run reads as a warning
  (amber), a successful run as good (green).
- Notifications can be sorted (by time or reason, with a direction toggle) and
  filtered by text, mirroring the task column.
- Server switcher: each server shows its unread count as a badge, so the one
  dashboard header doubles as a cross-server summary.
- Settings → Dashboard layout: show/hide the notifications column, the tasks
  column, and the time-logged bar.
- The dashboard now shows the last-known tasks and notifications from a locally
  cached first page immediately on launch, instead of empty columns, while the
  first refresh runs in the background.
- A sync indicator under the header shows whether a refresh is in progress, when
  the last successful sync happened, or that the server is unavailable and cached
  data is shown.
- Filters (tasks and notifications) support exclusion: a `-word` (or `!word`)
  term hides matching items, alongside plain include terms.
- A task or notification title is now clickable for every backend: it opens the
  in-app detail screen where available, otherwise the item's web page.
- Per-server setting "Open tasks & notifications in" (laba or Browser). The
  default follows the backend — OpenProject opens in laba, GitHub in the browser;
  servers opening in-app also offer a secondary "open in browser" control.
- GitHub notifications can now be marked read from the app: the read dot and
  "mark all read" work for GitHub too (via the GitHub REST API through `gh`).
- GitHub notifications now include already-read items (fetched with `all=true`),
  each carrying a read flag, so marking one read keeps it visible under the "All"
  view instead of removing it from the list.
- Notifications column opens on an Unread view and offers an Unread / All toggle,
  to triage handled items from those still pending; the toggle stays visible even
  when the current view is empty.
- The header settings entry is now a gear icon (with an accessible label) instead
  of the word "Settings".
- Settings search: a filter box at the top of Settings hides sections that do
  not match the query (matching legends, labels, and hints), like Chrome's
  settings search.
- GitHub task list: scope tabs "My repos" (default) and "Others" separate tasks
  in repositories you own from those you only follow, each with a count.
- Setup wizard: the GitHub URL field defaults to `github.com`, and when the `gh`
  CLI is authenticated the wizard shows which account on which host it is signed
  in as, so you can confirm the identity before adding the server.
- Server switcher: each enabled server has a refresh control to resync just that
  server on demand, with a spinner while the refresh runs.

### Changed
- The task list no longer transfers each task's description body; it is fetched
  on demand when the task's detail screen is opened (lazy loading).
- Backend capabilities are now a single nested object on the server info the GUI
  reads (one `capabilities` record instead of a growing list of flat `supports_*`
  booleans), with enums for the nuanced ones (one-way vs two-way read toggle,
  timelog with or without activities). Groundwork for additional backends.
- Tasks and notifications are now typed domain entities across the backend and
  the GUI (real `Task` / `Notification` shapes instead of open-ended records), so
  a field a backend cannot supply is explicit rather than a silent gap. Tasks
  carry why they are in the list (assigned, authored, review-requested, …) and a
  normalized status category; a task id is split into a display form and the raw
  id. GitHub pull requests whose review is requested from you now keep that
  reason even when they also match a broader search.
- GitHub is now the default backend for a new server (`server add` and the setup
  wizard), instead of OpenProject; OpenProject remains fully supported.
- Store OpenProject tokens through `keyring-core` with per-OS credential stores
  (Secret Service on Linux, Keychain on macOS, Credential Manager on Windows)
  instead of the `keyring` 3.x crate, following its 4.0 re-architecture. Existing
  tokens are migrated lazily on first read, so no re-login is needed.

### Fixed
- Dashboard: hide the time-logged indicator entirely when no enabled server
  supports time tracking (e.g. only GitHub servers are configured), instead of
  showing an empty "not configured" bar.

## [0.1.5] - 2026-07-14

### Changed
- Upgrade `reqwest` to 0.13. Keep the rustls backend on the `ring` crypto
  provider (installed explicitly via `rustls-no-provider`) — the same backend
  0.12 used — instead of 0.13's new `aws-lc-rs` default, which would add a C
  toolchain / NASM build dependency to the Windows and macOS release builds.
  Re-enable the now opt-in `query` feature.

### Added
- `THIRD-PARTY-LICENSES.md` listing every crate in the dependency graph with its
  SPDX license, generated by `scripts/gen-third-party-licenses.sh`.
- README demo (animated GIF walking the setup wizard and dashboard against
  anonymized mock data) with a screenshot gallery, reproducible via
  `gui/scripts/record-demo.sh`.

## [0.1.4] - 2026-07-13

### Added
- GitHub: surface everything that needs attention — issues/PRs I'm involved in (author, assignee, mention, comment), pull requests whose review is requested from me, and everything open in my own repositories — de-duplicated, instead of only items assigned to me.
- First-run setup wizard: a step-by-step modal (backend → connection → token → verify) that opens automatically when no server is configured. The GitHub step checks the `gh` CLI up front — offering an install link when it's missing, a sign-in prompt when it's unauthenticated, and a scope hint (`gh auth refresh -s repo,read:org,notifications`) for read-only logins.

## [0.1.3] - 2026-07-13

### Added
- Settings: sign in to an OpenProject server from the GUI — enter the API token in the add-server form or per server in the list, so a fresh install no longer needs the CLI to store a token. The token is validated against `users/me` and a duplicate account is rejected, mirroring `auth login`.
- Dashboard: an explicit empty state (with an "add a server" action) when no server is configured, instead of blank columns.
- Errors: show a friendly message instead of a raw backend string — strip the technical `kind:` prefix and, when a server has no token, show a "not signed in" notice with a link to Settings.
- Test coverage in CI: the GUI unit suite enforces a coverage threshold on the logic layer (`@vitest/coverage-v8`), and a `cargo llvm-cov` job reports Rust core/cli line coverage with a conservative floor.
- "Add a server" hint: a "Send a PR" link to the contributing guide alongside "Request a backend".
- `CONTRIBUTING.md` with a guide for adding a backend.

### Changed
- Settings: collapse the advanced per-server options (proxy, status colors, filters, display fields) into an "Advanced" section so the common fields stay uncluttered.
- Settings: confirm per-server profile edits with the same "Saved" indicator the global settings already showed.
- Task list: sort direction toggle (defaults to descending; click the active sort key to reverse).
- Localize time units (`ч`/`мин` in Russian) and the notification-count noun (Russian plural forms) instead of hardcoded `h`/`m` and a single suffix.
- Route cache, state, and retry warnings in the core library through the `log` facade (honoring `RUST_LOG`) instead of always printing to stderr.
- Document the `OPENPROJECT_SECRETS` environment variable and add a table of contents to the README.
- Format displayed dates (comment timestamps, timelog days) with the active locale via `Intl.DateTimeFormat` instead of a raw ISO slice.

### Security
- Warn at config load when a server disables TLS verification or uses a non-HTTPS `base_url`, since either can expose the API token.
- Create the fallback secrets file with 0600 permissions up front (and tighten a pre-existing loose file) instead of writing then chmod-ing.
- Pin third-party GitHub Actions (`dtolnay/rust-toolchain`, `Swatinem/rust-cache`, `taiki-e/install-action`, `rustsec/audit-check`, `tauri-apps/tauri-action`) to full commit SHAs with a version comment, so a rewritten upstream tag cannot alter the CI/release pipeline.
- Enforce a dependency license allow-list (`deny.toml`, `cargo deny check licenses` in the Audit workflow), so an update that pulls in a copyleft crate fails CI instead of shipping in a binary release.
- Drop `'unsafe-inline'` from the webview CSP `script-src` (Tauri hashes the bundled inline scripts, so no inline JS is silently trusted); verified the app still renders via the e2e smoke.

### Fixed
- Release: build the Linux CLI on the oldest supported Ubuntu (glibc parity with the GUI bundle) so the downloaded binary runs on older distributions instead of failing with a `GLIBC_2.3x not found` error.
- CLI: exit with the conventional `128 + signum` code on interruption (SIGTERM → 143, SIGHUP → 129) instead of always 130.
- CLI `--human` output: measure the key-column width in characters, not bytes, so a non-ASCII key no longer over-pads and skews the value column.

## [0.1.2] - 2026-07-11

### Fixed
- Linux AppImage: drop the bundled libwayland-client so the webview renders on modern distributions instead of showing a blank window.
- Tray: show the app icon with a small red count badge in the corner instead of replacing it with a bare number.

## [0.1.1] - 2026-07-11

### Changed
- Release bump to exercise the end-to-end self-update path from 0.1.0.

## [0.1.0] - 2026-07-11

### Added
- Cargo workspace: `core` library and `laba` CLI.
- Server profiles (JSON config) with default selection; SOCKS5/HTTP proxy per server.
- Token storage in the system keyring with a file fallback.
- `auth` (login/status/token/logout/import) and `server` (list/add/remove/set-default/show) commands.
- OpenProject and GitHub backends for tasks and time entries.
- Desktop GUI (Tauri v2 + Svelte 5): task list with status filters, task detail, time logging, tray indicator, notifications, and settings.
- Markdown rendering for task descriptions and comments.
- Signed self-update via the Tauri updater; multi-OS release bundles for Linux, Windows and macOS with a deterministic `latest.json` aggregator (macOS is notification-only).
- Settings migrations keyed by schema version.
