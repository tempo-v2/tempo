# Tempo 1 tactics metadata in Tempo 2

Tempo 1's catalog has 17,300 puzzle entries across 692 ordered packs, 47
primary themes, and six theme groups. The entries map to 17,271 distinct
Tempo 2 puzzles. Some source IDs describe identical positions and solutions,
so a puzzle can appear in more than one pack. The recovery keeps each source
entry and its original pack position. The remaining 386 puzzles in the
migration snapshot were personal captures. Their Tempo 1 capture records
contained source information and notes without structured theme or rating
fields.

| Tempo 1 pack label | Entries | Observed rating range |
| --- | ---: | ---: |
| Easy | 3,750 | 399–1100 |
| Medium | 2,800 | 1101–1500 |
| Hard | 2,800 | 1501–2000 |
| Focused | 7,000 | 1250–2000 |
| Advanced | 950 | 1741–2972 |

The pack theme and the Lichess tags serve different purposes. For example,
`mateIn4Plus` is a pack theme, and `mateIn5` is a tag on 272 distinct
puzzles across the catalog. The `hangingPiece` packs contain 550 distinct
puzzles, and the tag matches 823 across all packs. The 61 packs active in
the Tempo 1 snapshot are marked with `was_active`.

## Rebuild and query

Run from the assembled workspace with an existing private SQLite backup of
the imported Study dataset and a fresh output filename. The tool reads the
backup and checks each imported metadata record against its original pack
entry before creating the private catalog. Keep the inputs and output private.

```sh
python3 tempo/tools/tactics/recover_catalog.py \
  /private/path/study-backup.sqlite \
  .local/tempo1-migration/installed-data \
  .local/tempo1-migration/source.json \
  .local/tempo1-migration/puzzle-catalog.sqlite
```

These queries use the recovered file. The first reproduces Tempo 1's
pack order within a theme and difficulty. The second finds all mate-in-five
targets, using the lowest matching source rating when aliases share a target.

```sql
SELECT pack_id, position, rating, target_id
FROM puzzle_by_pack
WHERE theme = 'hangingPiece' AND difficulty = 'easy'
ORDER BY ordinal, position;

SELECT target_id, MIN(rating) AS rating
FROM pack_puzzle JOIN puzzle_tag USING (source_puzzle_id)
WHERE tag = 'mateIn5'
GROUP BY target_id
ORDER BY rating, target_id;
```

The recovered catalog describes the named backup and the supplied Tempo 1
snapshot. Tempo's live Study queue, scheduling and learning never read it.
