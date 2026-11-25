# Lichess rating packs

This tool builds a standalone SQLite catalog from the [CC0 Lichess puzzle
export](https://database.lichess.org/#puzzles). The pack rule comes from the
Tempo request for one pack per visible Lichess theme and 100-point rating band.
The offline catalog builder owns membership. It uses the export's puzzle ID,
rating, theme tags and a legal solution. Display labels and Study state do not
affect membership.

- Begin at 1000. A `1000–1100` band contains ratings 1000 through 1099.
  Continue in ascending bands with no fixed maximum.
- At the first band with fewer than 20 playable puzzles, use that band and
  higher ratings to fill one final pack, preferring the lowest remaining band.
  Emit a final pack only if it has 20 members. The catalog records the first
  short band and how many eligible candidates were available there and above,
  capped at 20. A sparse starting band can therefore produce just a `1000+`
  pack.
- Select in source row order within a band and present each pack in
  ascending rating order. The same source puzzle can satisfy several themes.
  It is stored once with several memberships. Pack identity is the theme plus
  the lower rating bound. Membership is reproducible for the pinned export.
- Use Lichess's visible themes, including the Healthy mix over the entire
  export. The Lichess opening-family and variation filters use a separate
  export column. Source-only tags are attribution, outside theme pack selection.

Tempo 1's collector streamed the compressed CSV and checked the setup and
solution plies for legality. Its expansion step allocated one global puzzle
ID to one pack and used 25-puzzle packs across broad tiers, including a
low/high split for named mates. The new catalog retains the setup move, the
solver position, the complete source line, the rating, and the source tags.
One puzzle may appear under both a motif and a named mate.

## Build

Install `zstd` and use Python 3.11 or later.

```sh
python3 -m venv .venv
.venv/bin/pip install python-chess==1.999
curl -fLO https://database.lichess.org/lichess_db_puzzle.csv.zst
.venv/bin/python tools/lichess-packs/build.py lichess_db_puzzle.csv.zst lichess-packs.sqlite3
.venv/bin/python -m unittest discover -s tools/lichess-packs
```

The output must be a new filename. Metadata includes the compressed source
SHA-256 and the revision of the Lichess theme definition. Query `themes` for
coverage, `packs` for band bounds and `pack_puzzles` for ordered membership.
The exclusive upper bound is NULL for a final pack. The `puzzles` table
contains answers and belongs on the server.

```sql
SELECT p.theme, p.rating_min, p.rating_end, MIN(z.rating), MAX(z.rating), COUNT(*)
FROM packs p
JOIN pack_puzzles m USING (theme, rating_min)
JOIN puzzles z USING (puzzle_id)
GROUP BY p.theme, p.rating_min
ORDER BY p.theme, p.rating_min;
```

## First play and Study

This file is a content artifact for the installed Tempo Lean profile. The
runtime still needs a blind Tactics attempt, an owned Learning transition and
a Store transaction to connect the two. The intended first-play contract is:

| Behavior | Owner and inputs | Independence and lifetime |
| --- | --- | --- |
| Browse packs and play in rating order. | Catalog serves membership. Tempo issues an attempt for the selected stable puzzle identity and content. Chess applies the setup and grades the whole solution. Lichess allows any actual mate for mate in 1. | Browsing, abandonment, label changes and overlapping memberships do not schedule a puzzle. The issued answer stays server-side. |
| The first accepted wrong move ends the scoring attempt and enters the puzzle into Study through the existing Again and repeat process. | Learning consumes the authoritative first-play result and existing learning identity. | A correction cannot turn a miss into Good. A puzzle appearing in another theme cannot admit it twice. |
| A first clean solve enters Study 7–20 local study days after acceptance, inclusive. | Learning initializes its ordinary Good memory and chooses the first due day once. | Store commits that day, the review and the receipt together. Exact retry returns the already accepted result and day. Existing learning takes precedence over membership. Later reviews use the normal schedule. A later export preserves the puzzle identities and content used by pending and accepted attempts. |

The runtime checks should compare a first Good and Again through Tactics,
then reopen and retry the same accepted command. Relabeling a theme or solving
through a second membership must leave the accepted grade and due date
unchanged. The catalog builder neither opens the installed database nor
updates its cards, queue or learning.

## Reference build

The export with SHA-256
`7b0d235fa40cfd7fdb7df1fec56a96b3b6ee1496b32ed2eb05be90d0ebf94253`
contains 6,193,530 data rows. It yielded 1,332 packs across all 74 visible
categories and 18,214 distinct Lichess IDs. Discovered Check produced 21
packs through 3000–3100. Pins produced a final 3100+ pack reaching 3271.
Castling and En Passant each start with a final 1000+ pack because their
first band is short.

## Lean implementation contract

Packs publish from an explicit catalog artifact. Puzzle IDs determine shared
first-play progress, and Chess determines solution versions and scoring.
An accepted wrong move or Reveal ends scoring as Again. A complete clean
line ends scoring as Good. Both Tactics and Study require the full line.
Tactics practice has an explicit mode and leaves Learning alone. A pack
refresh can replace membership and metadata. In-flight attempts and admitted
Study cards retain their version. Move receipts survive refresh and restart.
A display rename, another pack membership, or an imported game cannot change
a score or due date. Changing a legal solver move can change the score.

### Publish to Lean

Build the locked Lean binary and use the managed stopped-service lifecycle.
After taking a verified SQLite backup, migrate the retained database and run:

```sh
tempo import-lichess-packs /path/to/tempo.sqlite3 /path/to/lichess-packs.sqlite3
```

The command emits the catalog artifact's BLAKE3 identity and counts. The
release ledger also records the SHA-256 of the SQLite file. Repeating the
same import keeps its membership. A new artifact atomically replaces the
visible membership. For the reference catalog, expect 1,332 packs and 18,214
puzzle versions on first import. Reopen the app at **Tactics → Packs**. The
normal runtime needs only the installed database and bundled UI assets.
