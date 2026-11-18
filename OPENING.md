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

The combined release upgrades the opening database in place. The same start command resumes retained lessons and tests. Keep using the same SQLite path.
