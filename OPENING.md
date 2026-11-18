# Opening study

From the assembled workspace, run:

```sh
make start
```

Open [Tempo](http://127.0.0.1:41822). The launcher builds the locked Rust app and matching UI, then creates `.local/usable-release/study.sqlite` if it is missing. Restart with the same command and database to continue. Stop the foreground launcher with Ctrl+C.

Use **Repertoires** to upload or paste standard-start PGN, choose your learner color, preview the routes and save. To edit a repertoire, upload its complete intended PGN as a replacement. An unchanged answer keeps its learning. A changed answer needs a new lesson.

On a lesson, reproduce the shown move, then acknowledge it. Your first blind review of that answer becomes due the next study day. Study days start at midnight in America/New_York. Tempo serves due reviews in a stable random order within that day.

The default daily allowances are ten new opening positions and twenty new tactics. To set them independently, run this from the workspace, then restart Tempo:

```sh
target/debug/tempo configure .local/usable-release/study.sqlite 10 20
```

The third and fourth arguments are the opening and tactics limits. Use `make start TEMPO_PORT=41823` to select another loopback port. For a different SQLite file, create it with `target/debug/tempo init /path/to/study.sqlite` and select it with `make start TEMPO_DB=/path/to/study.sqlite`.
