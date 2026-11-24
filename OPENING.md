# Local study

On the Mac, run `make start` from the assembled workspace to build and restart
[Tempo](http://127.0.0.1:41822) with the current checkout and your retained study
database. The launcher replaces the registered service and closes verified
Tempo development servers on other ports. A new build is checked on a
disposable database copy before shutdown, and the launcher takes a separate
backup before switching artifacts.

Use **Repertoires** to import standard-start PGN or open **Edit** on a saved
repertoire. Edit the complete document, choose **Preview changes** to see
effects on the repertoire, then choose **Save repertoire**. **Download PGN**
fetches the current accepted document. If Tempo has no verified current PGN
for an older repertoire, upload a complete replacement to establish it.
An unchanged answer keeps its learning. A changed answer needs a new lesson.

Create a new repertoire by leaving the destination unselected. Rename an existing
repertoire by its current selection. A rename preserves its targets and learning.
A replacement retires omitted positions in the selected repertoire and retains
their history. Restoring a retired position compares its answer against the
latest retained version. A changed answer preserves its first introduction and
requires re-teaching without another introduction slot.

Daily Study accepts repertoire PGN through the signed Preview and Save commands.
The generic daily catalog method accepts active tactics only. The separate
standard profile retains additive imports, which keep omitted content present.

On a lesson, reproduce the shown move, then acknowledge it. Your first blind review of that answer becomes due the next study day. Study days start at midnight in America/New_York. Tempo serves due reviews in a stable random order within that day.

Set both daily allowances in desktop **Study settings**. The defaults are ten new opening positions and twenty new tactics.

Use **Tactic capture** to paste or upload a PGN. A single-line FEN snippet starts at the solver position. For a complete game, select the variation and move range. Preview the solver board and select which valid records to save. Tempo validates legality. Setup-bearing snippets can use the `TempoSetupPlies` tag or the preview's setup option.

## Development in the assembled workspace

`make start-dev` builds the locked backend and UI and runs them in the foreground
on [41823](http://127.0.0.1:41823). It initializes
`.local/development/study.sqlite` on the first run when `TEMPO_DB` is unset.
Running `make start-dev` again builds the application, stops the
verified Tempo development server on that port, and starts a new foreground
server. Ctrl+C stops it. If another application holds the port, choose a free
port with `TEMPO_PORT`.

```sh
make init-dev TEMPO_DB=/path/to/disposable.sqlite
make start-dev TEMPO_DB=/path/to/disposable.sqlite TEMPO_PORT=41824
make build-ui
```

An explicit database path must already exist for `make start-dev`. The preflight
checks the managed registration, database and build destinations before
development writes. `CARGO_TARGET_DIR` selects the development Cargo output
directory, and `tempo-ui/dist` is the development UI output. Use an assembled
checkout with the companion UI guard when adopting this tooling.

## Installed use on the Mac

Use this read-only status request. It reports today's remaining work without issuing an activity.

```sh
curl -fsS http://127.0.0.1:41822/api/study \
  -H 'Content-Type: application/json' \
  --data '{"command":"study_status"}'
```

The Mac checkpoint installation also has a session LaunchAgent at `.local/usable-release/com.tempo-v2.usable-release.plist`. The active profile points at a matching binary and UI under `checkpoints/` with the same database. The launch agent specifies the release log paths. To check or stop that managed instance, use the corresponding command below.

```sh
launchctl print "gui/$(id -u)/com.tempo-v2.usable-release"
launchctl bootout "gui/$(id -u)/com.tempo-v2.usable-release"
```

Use `make start` to install or restart the workspace build. To start the
already installed build without rebuilding, bootstrap the archived job if it
is unloaded or kickstart it if it is loaded and stopped.

```sh
launchctl bootstrap "gui/$(id -u)" "$PWD/.local/usable-release/com.tempo-v2.usable-release.plist"
launchctl kickstart "gui/$(id -u)/com.tempo-v2.usable-release"
```

## Phone Study and Build over Tailscale

Tempo Study opens the regular Study queue and uses the same saved progress as
desktop Tempo. Open the private HTTPS address in iPhone Safari, then use
**Share → Add to Home Screen** to keep it handy. The Mac must be awake and
reachable through Tailscale to start a session or submit a move.

The phone's **Build** tab opens saved coverage gaps and lets the configured
owner save a reviewed reply with a signed preview.

On the Mac, install the paired Tempo and UI build with the Serve HTTPS origin
and your exact Tailscale login. The managed registration saves both values for
future restarts. Read the hostname and login from the running Mac's Tailscale
account. This example uses port 8443 on the tailnet and the existing managed
Tempo port on loopback.

```sh
TEMPO_PHONE_ORIGIN=https://your-mac.your-tailnet.ts.net:8443 \
TEMPO_PHONE_TAILSCALE_LOGIN=you@example.com make start
tailscale serve --bg --https=8443 http://127.0.0.1:41822
tailscale serve status
```

Use Serve in tailnet-only mode. Validate the private URL before sharing it
with your devices. To take the phone URL offline, run
`tailscale serve --https=8443 off` before rolling back
the managed Tempo release. If unrelated disposable Tempo sessions are running,
set `TEMPO_KEEP_DEV_SERVERS=1` for this managed installation.

## Scheduling and settings

The runtime uses the upstream Rust FSRS crate pinned at `6.6.2` with its default parameter vector and 0.90 desired retention. The versioned policy and calendar settings are in SQLite. Actual reviews use the elapsed study days since the last accepted review. The scheduler rounds its returned intervals to whole study days, bounded from 1 to 36,500 days. A teaching acknowledgement schedules tomorrow's first blind review without creating FSRS memory.

Open **Tactics → Study settings** on desktop to edit both daily introduction
limits and choose themes for new tactics. Saved settings take effect on the
next selection. Tempo rotates across selected themes and works through the
original easy, medium, and hard packs for each theme. **Any tactic** includes
the full catalog and personal captures. Existing attempts and due reviews
retain their Study state.

## Personal captures

Successfully saved captures are introduced immediately and due for a first
review. Study resumes any exercise already on the board, then prioritizes
fresh personal captures over the regular review queue. Captures use no daily
new-tactic slots. Recapturing an already introduced puzzle preserves its
current review schedule.

## Backup and recovery

Run this from the workspace to make a consistent SQLite backup, including learned material, active work, receipts, and the saved profile. Keep the backup private.

```sh
umask 077
recovery_dir=$(mktemp -d "$PWD/.local/usable-release/recovery.XXXXXX")
sqlite3 -readonly .local/usable-release/study.sqlite ".backup '$recovery_dir/backup.sqlite'"
sqlite3 -readonly "$recovery_dir/backup.sqlite" 'PRAGMA integrity_check; PRAGMA foreign_key_check;'
```

A valid check prints `ok` with no foreign-key rows. Inspect or rehearse recovery
using a copy of the backup and the binary/UI pair verified for that backup's
schema. Find the current pair in the managed registration and release ledger.
For a schema-11 backup from the PGN-cap release, the matching checkpoint is
`workspace-c21029b32dd79bcfc7e2`. Use a separate port and a free database
path outside the UI directory. Keep the managed registration and installed
database in place during rehearsal.

```sh
checkpoint="$PWD/.local/usable-release/checkpoints/workspace-c21029b32dd79bcfc7e2"
cp -n "$recovery_dir/backup.sqlite" "$recovery_dir/recovered.sqlite"
"$checkpoint/tempo" serve "$recovery_dir/recovered.sqlite" "$checkpoint/ui" 41823
```

Open [the recovered copy](http://127.0.0.1:41823). Confirm the expected study state before choosing a recovery file for routine use. Keep the original database and backup intact. An older backup contains only the learning present at the time it was made.

Opening imports support standard-start orthodox games. Tactic capture accepts explicit setup instructions and selected game segments. It checks move legality and does not evaluate the solution with an engine.

## Historical Games and Study relevance

Historical Games backfill has ended. Saved games, previously recognized
player accounts, completed window effects and accepted correction provenance
remain. Recovery of an original accepted Start or Retry reports its archived
windows. Pending windows are stopped. Manual PGN import remains available in
Games. Previously accepted reprocessing commands can still replay their recorded results.

The latest 200 eligible games supply personal position counts. A confirmed
opponent gap opens the current repertoire at the position after the reply.
Play a legal response and choose Save response. Tempo previews the addition
against the current document and only signs changes that preserve existing
answers and learning. After saving, Tempo loads the next gap. Retained
source PGNs are required to confirm authored replies and endpoints.
