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

Choose **Reset progress** on a saved repertoire to erase its Study history and
review schedule. Confirm the repertoire by name. Its positions become new and
Study teaches them under the usual daily allowance. The PGN, other repertoires,
tactics and imported games are kept. Games played before the reset remain in
Games history and do not affect the new Study schedule.

Daily Study accepts repertoire PGN through the signed Preview and Save commands.
The generic daily catalog method accepts active tactics only. The separate
standard profile retains additive imports, which keep omitted content present.

On a lesson, reproduce the shown move, then acknowledge it. Your first blind review of that answer becomes due the next study day. Study days start at midnight in America/New_York. Tempo serves due reviews in a stable random order within that day.

The default daily allowances are ten new opening positions and twenty new tactics.

Use **Tactic capture** to paste or upload a PGN. A single-line FEN snippet starts at the solver position. For a complete game, select the variation and move range. Preview the solver board and select which valid records to save. Tempo validates legality. Setup-bearing snippets can use the `TempoSetupPlies` tag or the preview's setup option.

**Practice tactics** uses puzzles you have already learned and leaves their due dates alone. **Learn 10 more tactics today** adds to today's allowance. To customize that increment, pass it as the final argument to `tempo configure DATABASE OPENINGS TACTICS MORE_TACTICS`.

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

## Scheduling and settings

The runtime uses the upstream Rust FSRS crate pinned at `6.6.2` with its default parameter vector and 0.90 desired retention. The versioned policy and calendar settings are in SQLite. Actual reviews use the elapsed study days since the last accepted review. The scheduler rounds its returned intervals to whole study days, bounded from 1 to 36,500 days. A teaching acknowledgement schedules tomorrow's first blind review without creating FSRS memory.

Open **Tactics settings** from Study to change the daily introduction limit and
the themes used for new tactics. The saved selection takes effect on the next
new puzzle. Tempo rotates across selected themes and works through the original
easy, medium, and hard packs for each theme. **Any tactic** includes the full
catalog and personal captures. Existing attempts and due reviews retain their
Study state.

When rolling back to checkpoint 04 after saving a theme selection, stop Tempo
and make a current SQLite backup first. Checkpoint 04 can read the current
database after removing only the new profile field with
`UPDATE store_state SET local_profile=json_remove(local_profile,'$.tactic_themes') WHERE singleton=1;`.
Keep the current database for the rollback so it includes later Study progress.

For explicit installed maintenance, use the executable pinned by the LaunchAgent. This checkpoint 10 example sets 10 opening introductions, 20 tactics introductions and a 10-tactic extra increment. Apply the settings through the managed lifecycle.

```sh
.local/usable-release/checkpoints/10-repertoire-contract/tempo configure .local/usable-release/study.sqlite 10 20 10
```

## Personal captures

Successfully saved captures are introduced immediately and due for a first
review. Study resumes any exercise already on the board, then prioritizes
fresh personal captures over the regular review queue. Captures use no daily
new-tactic slots. Recapturing an already introduced puzzle preserves its
current review schedule.

For a dataset imported before this behavior was added, run
`tempo backfill-personal-tactics DATABASE` to check the counts and add
`--apply` to schedule the verified captures. Back up the dataset before
applying. The command records one receipt per dataset and leaves actual
grades to Study. Repeating the command returns the saved receipt.

## Backup and recovery

Run this from the workspace to make a consistent SQLite backup, including learned material, active work, receipts, and the saved profile. Keep the backup private.

```sh
umask 077
recovery_dir=$(mktemp -d "$PWD/.local/usable-release/recovery.XXXXXX")
sqlite3 -readonly .local/usable-release/study.sqlite ".backup '$recovery_dir/backup.sqlite'"
sqlite3 -readonly "$recovery_dir/backup.sqlite" 'PRAGMA integrity_check; PRAGMA foreign_key_check;'
```

A valid check prints `ok` with no foreign-key rows. To recover or inspect that backup, copy it into a new file and serve it on a separate port with the matching schema-3 checkpoint.

```sh
cp -n "$recovery_dir/backup.sqlite" "$recovery_dir/recovered.sqlite"
.local/usable-release/checkpoints/04/tempo serve \
  "$recovery_dir/recovered.sqlite" .local/usable-release/checkpoints/04/ui 41823
```

Open [the recovered copy](http://127.0.0.1:41823). Confirm the expected study state before choosing a recovery file for routine use. Keep the original database and backup intact. An older backup contains only the learning present at the time it was made.

Opening imports support standard-start orthodox games. Tactic capture accepts explicit setup instructions and selected game segments. It checks move legality and does not evaluate the solution with an engine.

## Historical Games and Study relevance

Games → Historical backfill takes public provider accounts and a fixed UTC date
range. It reads bounded provider windows and dated, matching stored PGNs.
Completed windows survive restart. Retry remaining windows uses the existing
job and its saved child receipts. Unlisted archives, truncated windows and
provider failures appear in the window details.

Accounts entered here stay recognized. Leave a provider blank to skip it
for this import. The first explicit account configuration scopes personal
game relevance and opponent gaps to the recognized identities.

Only this explicit historic job may add a new Game Again for a qualifying
unapplied miss from before a repertoire's starting reset. A later accepted
reset stops the job's remaining learning effects for that repertoire. Regular
refresh and stored-game reprocessing keep their original reset rules.
Distinct raw game and target identities retain their existing acceptance keys.

The latest 200 eligible games supply personal position counts. A confirmed
opponent gap opens the current repertoire at the position after the reply.
Play a legal response and choose Save response. Tempo previews the addition
against the current document and only signs changes that preserve existing
answers and learning. After saving, Tempo loads the next gap. Retained
source PGNs are required to confirm authored replies and endpoints.

To enable the local Rapid 1500 model, run `sh tempo/tools/maia/setup.sh PATH`
with `PATH` set to the absolute path of a `maia` directory next to the
active SQLite file. This uses Python 3.12, the pinned Maia-2 package, and
its checksum-verified Rapid checkpoint. Choose Analyze Maia probabilities
in Games. Inference persists full distributions as it progresses and can
reuse them after interruption. An unavailable probability stays unknown.
The displayed local coverage sums authored replies that have a saved learner
response. Study also uses a separate modeled reach across authored routes
where personal exposure is absent.

Opening a gap shows similar repertoire responses alongside a bounded
Stockfish search. Nearby positions are ranked by pawn files, piece placement
and castling rights. Tempo tries the most common reusable replies in the
current repertoire first, then other saved repertoires for the same side.

Tempo suggests the first reply within 50 centipawns of Stockfish's best.
If the best comparable repertoire reply is 51–150 centipawns behind, the
selection stays blank. Stockfish is suggested when every shortlisted reply
can be compared and is more than 150 centipawns behind. Missing evidence can
leave the selection blank. The arrow previews the suggestion on the original
board. Play a different move or clear the suggestion any time before saving.

The local comparison uses the existing Maia Python environment and the local
Stockfish executable. It examines up to eight distinct nearby positions per
group and compares up to sixteen distinct repertoire replies. Opening,
comparing, staging and clearing are read-only.

Lichess Explorer is optional. [Create a Lichess access token][explorer-token],
name it “Tempo Explorer” and leave the permissions unchecked. Save only the
token in a private plain-text file named `lichess-explorer.token` beside the
active SQLite database. The Explorer setup panel shows the exact file path and
can copy it. Choose **Retry Explorer** to pick up the file without restarting
Tempo. The server sends only the current board position and the selected Rapid
rating filter. You can remove the file or revoke the token in Lichess at any
time. Local analysis and repertoire saves are available independently.

[explorer-token]: https://lichess.org/account/oauth/token/create
