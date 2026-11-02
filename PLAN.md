# Waggle plan

Written 2026-11-02. Nothing here is built yet. This is the plan to pick up
when work starts.

## 1. What Waggle is, and why

Friends of ours in Goonswarm use RIFT for intel, and they're tired of how slow
and memory-hungry it is. The only things they use are **intel alerts,
Directorbot pings and their PAP count**. They'd also like the **mining
meter**. EVEmarchy already does all four well, but it only runs inside the
Omarchy desktop on Linux.

Waggle is those features, delivered in two forms over one engine:

- **Windows (and plain Linux):** a small native tray app.
- **Omarchy:** a bar widget and panel, like EVEmarchy's today, driven by the
  same engine running with no window.

Waggle covers:

| Feature | Notes |
|---|---|
| Intel alerts | Chat-log scanning, proximity to your characters via ESI location, jump bridges |
| Directorbot pings | Jabber client, notifications, mutable groups, replay suppression |
| PAP count | `!me` to Directorbot, parsed (see EVEmarchy `docs/pap-investigation.md`) |
| Mining meter | Yield from game logs, hold size from hull and skills, sessions and graph |
| Skill queue | Notifies when a skill finishes or the queue runs dry. Nearly free to include. |
| RIFT import | First-run, one-time import of Jabber login, intel channels and jump bridges |

Out of scope, and staying in EVEmarchy or moving to Nex Industries (NFA):

- Industry jobs, industry defaults, industry picks, corporation jobs: moving to NFA (§10).
- Client previews, Beehive and comms: these stay in EVEmarchy, which becomes
  a private add-on that runs on top of Waggle (§2).
- macOS: not a goal. Don't design it out, but don't test for it.

**Targets:** idle memory under 50 MB RSS, idle CPU under 1%, one installer,
working in under five minutes for someone coming from RIFT. Compare against
RIFT on the same machine and write down the numbers.

## 2. Architecture

One engine, one binary, several faces:

```text
 ┌──────────────────────────────────────────────────────────────┐
 │ waggle-core (library)                                        │
 │ logs · intel · bridges · universe · jabber · paps · esi/sso  │
 │ store · mining · skills · import · config                    │
 │ platform traits: secrets, notifications, file id, open url,  │
 │ paths                                                        │
 └───────────────────────────────┬──────────────────────────────┘
 ┌───────────────────────────────┴──────────────────────────────┐
 │ waggle (one binary)                                          │
 │   waggle         tray app + native UI (Windows, plain Linux) │
 │   waggle daemon  no window; serves the local API (Omarchy)   │
 └───────────────────────────────┬──────────────────────────────┘
                                 │ versioned local API
                                 │ (Unix socket + state snapshot)
          ┌──────────────────────┴───────────────────────┐
 ┌────────┴──────────────────┐   ┌───────────────────────┴──────┐
 │ waggle-omarchy            │   │ evemarchy (private add-on)   │
 │ QML only: bar widget,     │   │ client previews, Beehive,    │
 │ panel, toasts             │   │ comms: reads Waggle's state, │
 │ (this repo's root)        │   │ owns no Jabber or log work   │
 └───────────────────────────┘   └──────────────────────────────┘
```

### Each machine has exactly one owner

Exactly one Waggle process per machine owns the Jabber connection, log
tailing, ESI tokens and notifications. Everything else is a client of it.
Two owners would mean two Jabber logins on one account, double `!me` requests
to Directorbot, and every alert notified twice. The single-instance lock
(§5) enforces this, and neither EVEmarchy nor `waggle-omarchy` ever starts a
service of its own.

### Repository layout

This repo is both a Cargo workspace and an Omarchy plugin.
`omarchy plugin add <git-url>` clones the repo and expects `manifest.json`
at the root, so the plugin lives at the root and the Rust lives under
`crates/`. EVEmarchy is laid out the same way today.

```text
waggle/
  manifest.json         # Omarchy plugin manifest (waggle-omarchy)
  omarchy/              # the plugin's QML (if entryPoints accept a subdirectory; otherwise at the root)
  crates/waggle-core/   # portable logic, no UI, no platform shell-outs
    assets/             # universe, ore volumes and mining holds (generated from CCP's SDE)
  crates/waggle/        # the binary: tray app and `waggle daemon`
  crates/waggle-data/   # regenerates the assets (scripts/update-universe-data drives it)
```

The plugin and the binary share one version number, and they're released
together.

### Tray mode is one process; daemon mode adds the API

In tray mode, the UI and the background work live in one process: a Tokio
runtime does the work, and the UI thread reads from channels. There's no IPC
and no "daemon not running" state.

`waggle daemon` runs the same services with no window. It publishes the
EVEmarchy-style local API, the Unix socket (`src/ipc.rs`) and the atomic JSON
snapshot (`src/snapshot.rs`), both of which move into `crates/waggle`. This
API is a **public contract**: `waggle-omarchy` and the private EVEmarchy add-on
both depend on it. So it carries a version number and a written schema
(`docs/api.md`), and changes to it are additive. Keeping a narrow protocol
stable is easier than keeping the whole `waggle-core` library API stable for
an outside consumer.

The snapshot carries more than the Waggle UI needs, including per-client live
mining status (from `mining::live`) and character presence, because
EVEmarchy's previews read them. Without that data, EVEmarchy would have to tail
the logs itself and break the one-owner rule.

## 3. Phase 0: spikes and decisions (do these first, each is small)

**Status 2026-11-02: done.** Everything builds for Windows. Slint, the tray
icon and toasts work on Windows 11 using the **software renderer**; femtovg
needs OpenGL and fails in VMs. The EVE application is registered and the
licence is GPL-3.0. What each spike taught is in
[docs/windows-notes.md](docs/windows-notes.md).

1. **UI toolkit.** Recommendation: **Slint**. Its markup is close to QML,
   which we already know. It's retained-mode, so it idles at near-zero CPU, it
   has a small footprint, and its royalty-free licence covers desktop apps (it
   is also GPLv3). Fallback: **egui** (simplest, immediate-mode, with a
   slightly worse native look). Rejected: Tauri, because a webview is the kind
   of weight we're getting away from.
   *Spike:* a window plus a tray icon (`tray-icon` crate) plus one toast on
   Windows. Measure RSS. Pass if it's under 40 MB.
2. **Building for Windows.** The `xmpp` dependency uses `aws_lc_rs`, which
   needs CMake and NASM when built for Windows.
   *Spike:* build `waggle-core` for `x86_64-pc-windows-msvc` both with
   `cargo-xwin` locally and on a GitHub Actions `windows-latest` runner. If
   aws-lc is painful, try the `xmpp` feature set that uses `ring`. Note that
   `xmpp` comes from the `dotbear/xmpp-rs` fork (`answer-software-version`
   branch), so the fork has to build on Windows as well.
3. **A Windows machine to test on.** A VM is fine. We can cross-compile from
   Linux, but toasts, the tray, Credential Manager and log locking need real
   Windows to test.
4. **Register the Waggle EVE application** on developers.eveonline.com
   (Authorization Code with PKCE, no secret). Callback
   `http://127.0.0.1:<port>/callback`, scopes from §6. Decide whose account
   owns it, because that account keeps it alive.
5. **Where RIFT keeps its settings on Windows**, for the importer. On Linux,
   `src/import.rs` already knows the file format.
6. **Licence.** EVEmarchy has none. Waggle will be handed to other people, so
   pick one before the first release. NFA is GPL-3.0, and GPL-3.0 also works
   with Slint.

## 4. Phase 1: extract `waggle-core` from EVEmarchy

**Status 2026-11-02: done.** waggle-core holds logs and log discovery, intel
and alerts, universe and bridges, Directorbot parsing and the Jabber worker,
ESI login and presence, the store, the mining meter, skills, notifications'
wording, the RIFT importer, and the keyring (Secret Service, Windows
Credential Manager, in-memory). EVEmarchy builds on it from the
`waggle-core` branch, its behaviour unchanged except the RIFT identity
option, which was removed. 205 core tests pass on Linux and on Windows 11.
Departures from the plan below:

- Industry polling and the industry tables came along behind off-by-default
  switches (`IndustryPolling`, `store::Extras`) rather than being untangled
  first; they leave with industry for Nex Industries.
- The front end supplies an `AppIdentity` (name, version, website, required
  ESI scopes, relink hint), used by SSO, ESI's User-Agent, the login page and
  Jabber.
- Beehive's room became a generic `WatchedRoom` on the Jabber worker.

The aim is **zero behaviour change in EVEmarchy**. When this phase is done,
EVEmarchy builds against the core, its whole test suite passes, and
`waggle-core` builds and passes its tests on Windows CI.

### What moves into the core

From `evemarchy/src`:

| Module | Lines (approx.) | Notes |
|---|---|---|
| `logs/` (tailer, monitor, format, filename) | 930 | Polling, portable. Path discovery needs Windows roots (below). |
| `intel.rs`, `alert.rs` | 780 | Pure logic. |
| `universe.rs` + `assets/universe-v1.tsv` + `bin/update-universe.rs` | 1.3k | Pure logic. |
| `bridges.rs` | 180 | Pure logic. |
| `jabber.rs`, `jabber/wire_tests.rs`, `tests/fixtures/directorbot` | 2.3k | Portable once it builds (Phase 0, spike 2). |
| `esi/` (sso, callback, client, library) | 4.3k | Client has industry and asset endpoints. Split them out (below). |
| `store.rs` | 2.2k | Holds industry tables. Split the migrations (below). |
| `mining/` (mod, logs, model, holds, live) + its TSVs + `bin/update-mining-data.rs` | 2.8k | One Unix-only line (file identity). |
| `skills.rs` | 200 | Pure logic. |
| `import.rs` (RIFT) | 490 | Portable. Add the Windows location. |
| `config.rs`, `settings.rs`, `paths.rs` | 3.2k | Split (below). |
| `notify.rs`, `secrets.rs` | 1.2k | Become traits plus per-platform backends. |

What goes to `crates/waggle` (not the core) in Phase 2: `ipc.rs` and
`snapshot.rs`, minus their industry, Beehive and preview parts. The QML for the
in-scope features (the bar widget, the panel body, the mining panel and graph,
toasts, glyphs, the theme) goes to the plugin in Phase 2b.

What stays in EVEmarchy: `main.rs` (until Phase 4), `images.rs`,
`beehive.rs`, `comms.rs`, `demo.rs`, `doctor.rs`, `industry*.rs` (until NFA
takes them), and the preview QML.

During Phase 1 only, EVEmarchy links `waggle-core` as a library, so the
extraction can be checked against EVEmarchy's existing tests with no change
in behaviour. That link is temporary scaffolding: in Phase 4 EVEmarchy becomes
an API client instead.

### Dependency knots to untangle

Found by grepping `crate::` uses:

- **`notify.rs` depends on `industry` and `skills`.** Make notification
  *content* a core type with an open set of kinds, and have EVEmarchy add its
  industry kinds on top.
- **`esi/client.rs` depends on `mining`**, through the ship-type and skills
  reads for the hold. That's fine, since both are in the core. Move the
  industry, blueprint and asset endpoints into an EVEmarchy extension module.
- **`ipc.rs` depends on `mining`.** No change needed: `ipc` stays in
  EVEmarchy and depends on the core.
- **`config.rs` is one big struct** with `[industry]`, `[preview]` and
  `[beehive]` sections, and it uses `deny_unknown_fields`. Split it into a core
  `Config` (logs, settings, esi, intel, jabber, paps, notifications, mining)
  and an EVEmarchy `AppConfig` that flattens the core config in and adds its
  own sections. That keeps the existing `config.toml` files valid. Test this
  against the real `config.example.toml`.
- **`store.rs` migrations are numbered and include `industry_jobs` and
  `industry_off`.** The core owns its tables (`metadata`, `linked_characters`,
  `intel_alerts`, `jabber_pings`, `pap_snapshots`, `log_checkpoints`,
  `skill_queue` and the mining tables). EVEmarchy keeps its own migration
  sequence, in an `app_migrations` table, for the rest. Existing EVEmarchy
  databases must migrate without losing anything: test on a copy of the real
  `state.db`.
- **The ESI scope constants** (`esi/mod.rs`): the core defines the scopes for
  location, online status, ship type, skills and skill queue. EVEmarchy adds
  the industry, blueprint and asset scopes. Each front end asks for its own
  set.

### Platform seams to add

| Seam | Linux backend (keep current behaviour) | Windows backend |
|---|---|---|
| `SecretStore` | `secret-tool` (current) | Windows Credential Manager (`keyring` crate, `windows-native`) |
| `Notifier` | `notify-send` (current) | WinRT toast (`tauri-winrt-notification` or `notify-rust`) |
| File identity (mining `mod.rs:15`, `inode`/`device`) | `MetadataExt` | Volume serial plus file index (`file-id` crate) |
| Open URL (`comms.rs:165`, SSO) | `xdg-open` | `open` crate (or use it everywhere) |
| Log roots (`paths.rs`) | Proton, Flatpak, `~/Documents` | Known Folder `Documents\EVE\logs`. Must follow OneDrive's redirect, so use `directories::UserDirs::document_dir`, never `%USERPROFILE%\Documents`. |
| Config and state dirs | XDG | `%APPDATA%\Waggle` (config), `%LOCALAPPDATA%\Waggle` (state, logs) |
| Symlink test in `paths.rs` | as-is | `#[cfg(unix)]` |

Credential Manager has a size limit of 2560 bytes per secret, which EVE
refresh tokens and Jabber passwords fit inside easily. Check this in a test
anyway.

On Windows, EVE keeps its log files open while writing them. Rust's
`File::open` asks for read, write and delete sharing, and RIFT reads the same
files, so this should just work. Confirm it during the Windows spike.

### Order of work in Phase 1

1. Create the workspace here with an empty `waggle-core`, and point EVEmarchy
   at it.
2. Move the leaf modules first: `universe`, `bridges`, `logs`, `intel`,
   `alert`, `skills`, `mining/{logs,model,holds,live}`. Re-export them from
   EVEmarchy's `lib.rs` so its call sites don't change yet.
3. Add the platform traits (secrets, notifications), then move `secrets` and
   `notify` behind them.
4. Split the config, then the store migrations, then the ESI client.
5. Move `jabber`, `esi`, `mining/mod`, `import`.
6. Set up CI: `cargo test -p waggle-core` on Ubuntu and Windows, and
   EVEmarchy's tests on Ubuntu.

Each step is its own commit and leaves EVEmarchy green.

## 5. Phase 2: the Waggle app

### Runtime

- One process. A Tokio multi-thread runtime runs the core services (log
  monitor, intel, Jabber, ESI poller, mining, skills). The UI runs on the main
  thread.
- One instance only: a named mutex on Windows, a lock file on Linux. Starting
  a second copy brings the first one's window forward.
- Optional start at login: a registry `Run` key on Windows, XDG autostart on
  Linux (`auto-launch` crate). Ask during first run.
- Logs: rotating files in the state directory, with a "Copy diagnostics"
  button that bundles them for whoever is helping debug.

### First run (the "under five minutes" path)

1. **Import from RIFT?** Detect RIFT's settings file. If it's there, import the
   Jabber login, intel channels and jump bridges, then show what came across.
   The import runs once: tokens are never read and RIFT is never touched again
   (see `import.rs` for why).
2. **EVE log folder.** Auto-detect it, show the path, and let the user change
   it.
3. **Add characters.** One "Add character" button per character, using the SSO
   flow in §6.
4. **Jabber.** Confirm the login, then do a test connection and a PAP fetch, so
   the user sees a number before they leave the wizard.

### Main window

Small, can be pinned, dark by default:

- **Header:** PAP counts (month, 30 days, 90 days) and Jabber connection status.
- **Intel feed:** system, jumps from the nearest character, who reported it,
  age. Rows fade as they get old, using `intel.freshness_seconds`.
- **Pings feed:** the latest Directorbot broadcasts, with muted groups hidden.
- **Mining:** the current session's yield, hold fill and a graph, ported from
  `EveMiningPanel.qml` and `EveMiningGraph.qml`. Session controls are start,
  stop, new and rename.
- **Characters:** online or offline, current system, and skill queue state.
- **Settings:** intel channels, alert range in jumps, cooldown, muted ping
  groups, privacy mode, notification sounds, start at login.

### Tray

- The icon shows a state: normal, intel nearby, or disconnected. The tooltip
  carries the PAP count and a one-line status.
- Menu: open, pause alerts for 15m/1h, refresh PAPs, quit.
- Closing the window hides it to the tray instead of quitting.

### Notifications

- Intel nearby, a fleet ping, a skill finished, the queue running dry, the
  mining hold full.
- On Windows, use the toast's built-in sounds (`ms-winsoundevent:`) so we
  don't need an audio library. Use a distinct sound for intel, because RIFT
  users rely on hearing it.
- **Windows gotcha:** toasts from an app that isn't packaged as an MSIX need an
  AppUserModelID registered through a Start Menu shortcut. The installer has
  to create that shortcut, or toasts silently do nothing.

### `waggle daemon`

- The same services as tray mode, with no UI, plus the local API (§2). On
  Linux it runs as a systemd user service started with the graphical session,
  like `packaging/systemd/evemarchy.service`.
- It sends notifications through the core's `Notifier` (`notify-send` on
  Omarchy), so they fire even when the bar is hidden.
- Login, settings and the RIFT import use CLI subcommands (`waggle auth add`,
  `waggle import rift`, `waggle config check`), as EVEmarchy does today. The
  panel can launch them.

## 5b. Phase 2b: the Omarchy plugin (`waggle-omarchy`)

QML only, ported from EVEmarchy's widget, cut down to Waggle's features:

- **Bar widget:** PAP count and an intel-nearby state.
- **Panel:** intel feed, pings, mining (panel and graph), characters and
  skill queue, plus buttons that send commands over the API (refresh PAPs,
  rescan logs, mining session controls).
- **Toast host** for in-shell alerts, styled by the Omarchy theme.
- **"Waggle isn't running" state:** if the socket is missing, the widget says
  so and shows the install command, instead of a blank widget.

Omarchy facts learned building EVEmarchy, which apply here too:
- Editing a bar widget's QML needs a full shell restart. The "plugin
  reloaded" message doesn't mean the widget picked up the change.
- The panel closes when it loses focus, so pin it before taking a screenshot.
- `qmllint` prints nothing and exits 255, so check QML errors in the journal
  instead.

Installation on Omarchy has two parts, and the README must say both:
1. The binary: an AUR package (`waggle-bin`) or a release download, plus the
   systemd user unit.
2. The plugin: `omarchy plugin add https://github.com/dotbear/waggle --enable`.

## 5c. Phase 4: EVEmarchy becomes an add-on

Do this after Waggle's Omarchy plugin works on your own machine:

1. Point EVEmarchy's previews, Beehive and comms at Waggle's API, so they read
   presence and live mining status from the snapshot.
2. Delete EVEmarchy's own services (logs, intel, Jabber, ESI, PAP, mining,
   skills) and its link to `waggle-core`. Industry should have gone to NFA by
   now (§10).
3. Migrate your existing data once: a `waggle import evemarchy` command that
   copies `~/.config/evemarchy/config.toml` (the sections Waggle covers), the
   keyring entries and the relevant `state.db` tables into Waggle's paths.
4. Strip EVEmarchy's bar widget down to what the add-on shows, or drop it if
   the previews need no widget.
5. **Open question:** Beehive uses ESI. If it needs scopes outside Waggle's
   minimal set, either Waggle requests them when the add-on is installed, or
   the add-on runs its own SSO for those scopes only. Don't add scopes to every
   Waggle user's consent page for one private add-on.

Once Phase 4 is done, EVEmarchy can be published. Its git history contains
personal details that shouldn't be public, so publish from a fresh history,
never by switching the existing repo to public.

## 6. ESI login (SSO)

We compared this against RIFT's implementation (public fork:
`github.com/AlexanderJohnston/RIFT`, `src/main/kotlin/dev/nohus/rift/sso/`).
Both apps use the same protocol (Authorization Code with PKCE, no client
secret, local callback, signed-token check), so the flow stays as EVEmarchy
has it. What changes is how it's packaged.

**Taken from RIFT:**

- **A built-in client ID.** Users never visit developers.eveonline.com. The
  config can still override the ID. Today EVEmarchy asks each user to register
  their own app (`config.example.toml:14`), which would lose people at step
  one.
- **Login is a button**, the browser opens by itself, and the callback page is
  friendly and on-brand.

**Kept from EVEmarchy, where it does better than RIFT:**

- **Listen on `127.0.0.1` only.** RIFT's Netty server appears to listen on
  every interface while it waits.
- **The redirect uses a literal `127.0.0.1`, not `localhost`.** When
  `localhost` resolves to IPv6 `::1` and the server only listens on IPv4, the
  redirect is lost. This is a plausible cause of RIFT's "told me to close the
  page but never connected" reports. That's unconfirmed, but either way it's a
  failure we can avoid.
- **A timeout and a Cancel button.** A mismatched `state` gets an error page
  instead of being silently ignored.
- **Tokens in the OS keystore** (Credential Manager or Secret Service), not in
  a settings file.

**Better than both:**

- A port that's already taken gets a clear message ("another program is using
  port N"), not a hang. EVE needs the exact registered callback URL, so the
  port is fixed. Pick one that isn't RIFT's 25252, so both apps can run
  together during migration.
- `invalid_grant` on refresh (the user revoked access, or the token expired)
  marks the character **"needs login again"** in the UI. RIFT silently deletes
  the character instead.
- **Minimal scopes**, so the EVE consent page is short:
  `esi-location.read_online.v1`, `esi-location.read_location.v1`,
  `esi-location.read_ship_type.v1`, `esi-skills.read_skills.v1`,
  `esi-skills.read_skillqueue.v1`. No industry, wallet, asset or waypoint
  scopes.

## 7. Jabber and Directorbot etiquette

- Each user brings their own goonfleet Jabber login. It's imported from RIFT
  or typed in, and stored in the keystore.
- Waggle answers `jabber:iq:version` as **Waggle** and its version. It must
  never pretend to be RIFT (see `docs/pap-investigation.md`: the `rift`
  identity turned out to be unnecessary and was reverted).
- Keep EVEmarchy's PAP request pacing and pause logic (`PapRequests`). More
  users means more `!me` traffic to Directorbot, so stay polite: fetch on
  start, on demand, and at a modest interval.
- Consider telling GSF tech or the Directorbot operators that Waggle exists
  before it spreads beyond a handful of friends.

## 8. Phase 3: packaging, releases, updates

- **Windows:** an installer (NSIS through `cargo-packager`, or Inno Setup) that
  installs per user with no admin, creates the Start Menu shortcut (needed for
  toasts, §5), and offers start at login. Also a portable zip for people who
  want one.
- **Signing:** ship unsigned at first. Friends will see SmartScreen's "Windows
  protected your PC" warning, so document "More info, then Run anyway". If
  Waggle spreads, sign with Azure Trusted Signing (about $10 a month) or an OV
  certificate.
- **Omarchy:** an AUR package (`waggle-bin`) containing the binary and the
  systemd user unit, plus the plugin from this repo (§5b). The release tag
  bumps both `Cargo.toml` and `manifest.json`.
- **Plain Linux:** the tray app as a plain binary or AppImage. It comes almost
  for free if the UI toolkit is cross-platform. Nice to have, not a goal.
- **CI:** GitHub Actions runs the tests on Ubuntu and Windows for every push. A
  tag builds the Windows installer and zip and attaches them to a GitHub
  release.
- **Updates:** at first, check GitHub releases once a day and show "Waggle X.Y
  is available" with a link. No auto-updater for now.

## 9. Testing

- The core keeps EVEmarchy's tests: Directorbot fixtures, the loopback XMPP
  server, and chat-log and game-log fixtures. All of them run on Windows CI.
- Add Windows-path fixtures for log discovery, including a OneDrive-redirected
  Documents folder.
- Do a manual Windows checklist before each release: install, first run with a
  RIFT import, SSO, an intel toast with sound, a ping toast, a PAP fetch, a
  mining session, tray behaviour, start at login, uninstall.
- Measure memory and CPU against RIFT on the same machine, and record the
  numbers in the release notes.

## 10. Related work: industry moves to Nex Industries (NFA)

This is NFA's job, not Waggle's, but it's sketched here so the split is
recorded in one place:

1. Move industry-job sync and completion notifications into NFA (a
   GenServer), then delete `industry.rs`, `industry_defaults.rs` and
   `industry_picks.rs` from EVEmarchy.
2. Run NFA as a real service instead of the dev server: a `mix release` as a
   systemd user service, with Postgres as a normal service.
3. Give NFA its own Omarchy bar plugin (free slots, job timers) that opens the
   UI in a webapp window. NFA stays Linux-only.

Phase 1 doesn't depend on this: the industry modules can stay in EVEmarchy
until NFA is ready.

## 11. Open questions

- Is Slint confirmed by the Phase 0 spike, or do we fall back to egui?
- Whose EVE developer account owns the Waggle SSO application?
- Which licence?
- Public GitHub releases, or a private link shared with friends?
- Do Omarchy plugin `entryPoints` accept a subdirectory (`omarchy/BarWidget.qml`),
  or must the QML sit at the repo root? Check with `omarchy plugin validate`.
- Does the plain-Linux tray build ship, or is Omarchy the only Linux target?
- Which ESI scopes does Beehive need, and where do they come from (§5c)?
- An icon and visual identity (bee and waggle-dance motif, Goonswarm colours
  without using alliance logos).
