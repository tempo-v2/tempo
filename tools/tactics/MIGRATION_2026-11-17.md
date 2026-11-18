# Installed Tempo 1 tactics transfer on November 17, 2026

The source was the installed Tempo 1 PostgreSQL schema 65 and the catalog assets
from the running web image at revision
`d79ae4fcc4b006985114d85b5fb9a5aca8becbf1`. The read-only, repeatable-read
snapshot was taken at 2026-11-18 02:35:53 UTC. The destination was the SQLite
dataset opened by the Tempo 2 server at `127.0.0.1:41822`.

| Source and outcome | Count |
| --- | ---: |
| Installed packaged puzzle records across 692 packs | 17,300 |
| Current tactical card rows in PostgreSQL | 1,901 |
| Legacy deletion tombstone | 1 |
| Older deck records matching installed pack rows exactly | 7,150 |
| Total definition records and deletion tombstone | 26,352 |
| Unique valid puzzle definitions | 17,657 |
| Duplicate definitions among source records | 8,691 |
| Already present in Tempo 2 before the import | 0 |
| Newly imported | 17,657 |
| Archived variants and deletion tombstone excluded | 3 |
| Invalid stored variant excluded | 1 |
| Final active Tempo 2 catalog | 17,657 |

The duplicate count includes 1,541 current card or pack records and 7,150
older deck copies. All 61 activated packs had underlying installed assets.
The legacy database `puzzles` table and the relevant card revision selection
were empty. The ledger also resolves 401 capture references on 386 cards,
1,516 progress references, and 1,517 distinct observed puzzle IDs. All of
these references point to retained puzzle content. All 401 capture requests
matched their linked cards' solver positions and full lines. The captures
included 286 Puzzle Rush, 73 manual, 25 game, and 17 other captures. Source
URLs identify both Chess.com and Lichess. Derived opportunities attached to
game analysis remained outside the puzzle catalog.

## Excluded variants

| Tempo 1 record | Disposition |
| --- | --- |
| Card beginning `14e0b25a3a06` | The stored position and retained issuance position both make ply 1, `a2a8`, illegal. The original pack exercise `07046` was imported from its valid position. |
| Cards beginning `44b8efb88a34` and `a06234fc4367` | The stored cards were archived. The original pack exercises `2rE0J` and `00Kq4` were independently validated and imported. |
| Card beginning `ff55c784bd77` | Tempo 1 retained a deletion tombstone and no current card. The retained origin has no pawn on b3 for its first move, `b3b4`. |

The private ledger contains complete source IDs and retained target IDs.
The only irrecoverable board/line pairs were invalid legacy variants. The
available catalog originals are playable.

## Installed verification

- The same server process read the catalog immediately after the commit.
  `study_status` returned 20 outstanding tactics and 20 introductions
  remaining. The dataset identity and stored profile matched the pre-import
  backup.
- SQLite reported 17,657 active items with the default unintroduced learning
  state. The live database had zero attempts, assessments, review receipts, or
  extra-introduction allowances after migration. Integrity and foreign-key
  checks passed.
- The second live import found all 17,657 targets already present and added
  zero. A separate disposable test repeated the import after grading and
  imported an alternative source ID for a learned puzzle. Both left that
  SQLite database byte-identical.
- Through the HTTP application on disposable databases, normal study issued
  and accepted 20 distinct new tactics, then stopped with four still
  unintroduced. Legal full-line solves covered white and black multi-move
  puzzles (`00008`, `0000D`), promotions for both colors (`001w5`, `004LZ`),
  castling (`01xdm`), en passant (`02p9D`), and a 10-ply captured line ending
  in an opponent reply.
- Three Tempo application tactics contracts, three Chess compiler tests, and
  three converter unit tests passed.

Private evidence is under `.local/tempo1-migration/` in the assembled
workspace. The raw PostgreSQL export (`source.json`) has SHA-256
`c47bf6b6c830827deee3798365dca55ac4a51dd8f58fae85e5ecdaeb7c8c8fae`. The
per-record report (`live-reconciliation.json`) has SHA-256
`05c4ecdabc5a40b9baaae017cf3da81b882071840d0f57bf3f237ba952a64bd8`.
The same directory holds the before and after backups, the checksummed raw
catalog bundle, and the live rerun report.
