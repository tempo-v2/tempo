# Local study

Run this from the assembled workspace.

```sh
make start
```

Open [Tempo](http://127.0.0.1:41822). The launcher builds the locked Rust app and matching UI, then creates `.local/usable-release/study.sqlite` if it is missing. Restart with the same command and database to continue. Stop the foreground launcher with Ctrl+C.

Use **Repertoires** to upload or paste standard-start PGN, choose your learner color, preview the routes and save. To edit a repertoire, upload its complete intended PGN as a replacement. An unchanged answer keeps its learning. A changed answer needs a new lesson.

On a lesson, reproduce the shown move, then acknowledge it. Your first blind review of that answer becomes due the next study day. Study days start at midnight in America/New_York. Tempo serves due reviews in a stable random order within that day.

The default daily allowances are ten new opening positions and twenty new tactics. To set them independently, run this from the workspace, then restart Tempo.

```sh
target/debug/tempo configure .local/usable-release/study.sqlite 10 20
```

The third and fourth arguments are the opening and tactics limits. Use `make start TEMPO_PORT=41823` to select another loopback port. For a different SQLite file, create it with `target/debug/tempo init /path/to/study.sqlite` and select it with `make start TEMPO_DB=/path/to/study.sqlite`.

Use **Tactic capture** to paste or upload a PGN. A single-line FEN snippet starts at the solver position. For a complete game, select the variation and move range. Preview the solver board and select which valid records to save. Tempo validates legality. Setup-bearing snippets can use the `TempoSetupPlies` tag or the preview's setup option.

**Practice tactics** uses puzzles you have already learned and leaves their due dates alone. **Learn 10 more tactics today** adds to today's allowance. To customize that increment, pass it as the final argument to `tempo configure DATABASE OPENINGS TACTICS MORE_TACTICS`.

The same start command resumes retained lessons and tests from `.local/usable-release/study.sqlite`. Keep using that path for ordinary study.

## Running and checking Tempo on the Mac

Run these commands from `/Users/andy/Developer/tempo-v2`. `make start` builds with the workspace Cargo lock and the UI npm lock. After both builds succeed, it stops this checkout’s Tempo servers across ports, including archived checkpoint builds. It then serves the selected database in the foreground and logs to the terminal. Ctrl+C stops it. If another application is using the requested port, the launcher reports the conflict before stopping any server.

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

To start the managed instance, stop any foreground listener first. Bootstrap the archived job if it is unloaded, or kickstart it if it is loaded and stopped.

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

Run the archive's executable for configuration if you have not built the debug executable. For example, the following sets 10 opening introductions, 20 tactics introductions, and a 10-tactic extra increment. Restart the same instance to apply the new allowances.

```sh
.local/usable-release/checkpoints/06/tempo configure .local/usable-release/study.sqlite 10 20 10
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

Open [the recovered copy](http://127.0.0.1:41823). Confirm the expected study state before choosing a recovery file for routine use. Keep the original database and backup intact. To use the recovered copy on port 41822, run `make start TEMPO_DB="$recovery_dir/recovered.sqlite"`. An older backup contains only the learning present at the time it was made.

Opening imports support standard-start orthodox games. Tactic capture accepts explicit setup instructions and selected game segments. It checks move legality and does not evaluate the solution with an engine.
