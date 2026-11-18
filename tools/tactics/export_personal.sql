-- Schema 65+ Tempo PostgreSQL. Stream a single JSON record with psql -XqAt
-- -v ON_ERROR_STOP=1. Avoid a connection that can write to the source.
\if :{?content_only}
\else
\set content_only false
\endif
BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY;
SET LOCAL search_path TO pg_catalog, public;
WITH RECURSIVE
candidate_cards(id) AS (
    SELECT id FROM public.cards WHERE content_type IN ('tactic', 'tactics')
    UNION
    SELECT card_id FROM public.tactic_captures
    UNION
    SELECT card_id FROM public.tactic_progress WHERE card_id IS NOT NULL
    UNION
    SELECT card_id FROM public.queue_attempt_origins WHERE content_type IN ('tactic', 'tactics')
),
-- Preserve supersession links, even when a linked replacement lives elsewhere.
card_ids(id) AS (
    SELECT id FROM candidate_cards
    UNION
    SELECT c.superseded_by FROM public.cards c JOIN card_ids ids ON c.id = ids.id
    WHERE c.superseded_by IS NOT NULL
),
relevant_reviews AS (
    SELECT r.* FROM public.reviews r JOIN card_ids ids ON r.card_id = ids.id
),
relevant_receipts AS (
    SELECT r.* FROM public.review_attempt_receipts r
    WHERE r.card_id IN (SELECT id FROM card_ids)
       OR r.review_id IN (SELECT id FROM relevant_reviews)
),
relevant_operations AS (
    SELECT o.* FROM public.operation_receipts o
    WHERE o.command_name IN ('tactics.capture.create', 'tactics.attempt.submit', 'tactics.activation.set')
       OR (
           o.command_name IN ('cards.review', 'cards.review.reconcile')
           AND (
               CASE WHEN o.payload_json IS JSON OBJECT THEN o.payload_json::jsonb ->> 'card_id' END
               IN (SELECT id FROM card_ids)
               OR o.operation_id IN (SELECT attempt_id FROM relevant_receipts)
           )
       )
),
linked_repertoires AS (
    SELECT repertoire_id FROM public.cards WHERE id IN (SELECT id FROM card_ids)
    UNION
    SELECT repertoire_id FROM public.repertoire_cards WHERE card_id IN (SELECT id FROM card_ids)
),
raw_rows(relation, row_data) AS (
    SELECT 'candidate_card_ids', to_jsonb(t) FROM candidate_cards t
    UNION ALL
    SELECT 'card_ids', to_jsonb(t) FROM card_ids t
    UNION ALL
\if :content_only
    SELECT 'cards', to_jsonb(t) - ARRAY[
        'ease', 'state', 'lapses', 'due_date', 'stability', 'repetitions',
        'introduced_at', 'fsrs_card_json', 'interval_days', 'first_correct_at',
        'maximum_interval', 'hard_correct_streak', 'recent_attempts_json',
        'unlock_after_card_id', 'reinforcement_pending', 'scheduling_mode',
        'prefix_split_rejected_after_review_id'
    ] FROM public.cards t WHERE t.id IN (SELECT id FROM card_ids)
    UNION ALL
    SELECT 'deleted_cards', jsonb_build_object('card_id', t.card_id)
    FROM public.deleted_cards t WHERE t.card_id IN (SELECT id FROM card_ids)
    UNION ALL
    SELECT 'tactic_captures', to_jsonb(t) - ARRAY['captured_at', 'result_json']
    FROM public.tactic_captures t
    UNION ALL
    SELECT 'tactic_progress', jsonb_build_object('puzzle_id', puzzle_id, 'card_id', card_id, 'deck_id', deck_id)
    FROM public.tactic_progress
    UNION ALL
    SELECT DISTINCT 'observed_puzzle_ids', jsonb_build_object('puzzle_id', puzzle_id)
    FROM (
        SELECT puzzle_id FROM public.tactic_discovery_attempts
        UNION
        SELECT puzzle_id FROM public.tactic_introductions
    ) observed
    UNION ALL
    SELECT 'tactic_pack_activation', to_jsonb(t) FROM public.tactic_pack_activation t
    UNION ALL
    SELECT 'puzzles', to_jsonb(t) FROM public.puzzles t
    UNION ALL
    SELECT DISTINCT 'queue_attempt_origins', jsonb_build_object(
        'card_id', card_id, 'start_fen', start_fen, 'moves_json', moves_json
    ) FROM public.queue_attempt_origins WHERE card_id IN (SELECT id FROM card_ids)
\else
    SELECT 'cards', to_jsonb(t) FROM public.cards t WHERE t.id IN (SELECT id FROM card_ids)
    UNION ALL
    SELECT 'card_revisions', to_jsonb(t) FROM public.card_revisions t WHERE t.card_id IN (SELECT id FROM card_ids)
    UNION ALL
    SELECT 'deleted_cards', to_jsonb(t) FROM public.deleted_cards t WHERE t.card_id IN (SELECT id FROM card_ids)
    UNION ALL
    SELECT 'repertoires', to_jsonb(t) FROM public.repertoires t WHERE t.id IN (SELECT repertoire_id FROM linked_repertoires)
    UNION ALL
    SELECT 'repertoire_cards', to_jsonb(t) FROM public.repertoire_cards t WHERE t.card_id IN (SELECT id FROM card_ids)
    UNION ALL
    SELECT 'tactic_captures', to_jsonb(t) FROM public.tactic_captures t
    UNION ALL
    SELECT 'tactic_progress', to_jsonb(t) FROM public.tactic_progress t
    UNION ALL
    SELECT 'tactic_discovery_attempts', to_jsonb(t) FROM public.tactic_discovery_attempts t
    UNION ALL
    SELECT 'tactic_introductions', to_jsonb(t) FROM public.tactic_introductions t
    UNION ALL
    SELECT 'tactic_pack_activation', to_jsonb(t) FROM public.tactic_pack_activation t
    UNION ALL
    SELECT 'tactic_rotation', to_jsonb(t) FROM public.tactic_rotation t
    UNION ALL
    SELECT 'puzzles', to_jsonb(t) FROM public.puzzles t
    UNION ALL
    SELECT 'puzzle_decks', to_jsonb(t) FROM public.puzzle_decks t
    UNION ALL
    SELECT 'reviews', to_jsonb(t) FROM relevant_reviews t
    UNION ALL
    SELECT 'review_attempt_receipts', to_jsonb(t) FROM relevant_receipts t
    UNION ALL
    SELECT 'operation_receipts', to_jsonb(t) FROM relevant_operations t
    UNION ALL
    SELECT 'review_schedule_snapshots', to_jsonb(t) FROM public.review_schedule_snapshots t
    WHERE t.review_id IN (SELECT id FROM relevant_reviews UNION SELECT review_id FROM relevant_receipts)
    UNION ALL
    SELECT 'queue_attempt_origins', to_jsonb(t) FROM public.queue_attempt_origins t WHERE t.card_id IN (SELECT id FROM card_ids)
    UNION ALL
    SELECT 'daily_queue', to_jsonb(t) FROM public.daily_queue t WHERE t.card_id IN (SELECT id FROM card_ids)
    UNION ALL
    SELECT 'study_attempts', to_jsonb(t) FROM public.study_attempts t WHERE t.card_id IN (SELECT id FROM card_ids)
    UNION ALL
    SELECT 'legacy_content_historical_origins', to_jsonb(t) FROM public.legacy_content_historical_origins t
    WHERE t.legacy_target_id IN (SELECT id FROM card_ids)
\endif
    UNION ALL
    SELECT 'tempo_schema_migrations', to_jsonb(t) FROM public.tempo_schema_migrations t
),
raw_tables AS (
    SELECT relation, jsonb_agg(row_data ORDER BY row_data) AS rows, count(*) AS row_count
    FROM raw_rows GROUP BY relation
),
exported_tables(name) AS (
    VALUES ('candidate_card_ids'), ('card_ids'), ('cards'), ('card_revisions'), ('observed_puzzle_ids'),
           ('deleted_cards'), ('repertoires'), ('repertoire_cards'), ('tactic_captures'),
           ('tactic_progress'), ('tactic_discovery_attempts'), ('tactic_introductions'),
           ('tactic_pack_activation'), ('tactic_rotation'), ('puzzles'), ('puzzle_decks'),
           ('reviews'), ('review_attempt_receipts'), ('operation_receipts'), ('review_schedule_snapshots'),
           ('queue_attempt_origins'), ('daily_queue'), ('study_attempts'),
           ('legacy_content_historical_origins'), ('tempo_schema_migrations')
)
SELECT jsonb_build_object(
    'schema_version', 1,
    'source', jsonb_build_object(
        'format', 'tempo-postgresql-tactics',
        'database', current_database(),
        'server_version', current_setting('server_version'),
        'transaction_read_only', current_setting('transaction_read_only'),
        'transaction_isolation', current_setting('transaction_isolation'),
        'captured_at', transaction_timestamp(),
        'migration_version', (SELECT max(version) FROM public.tempo_schema_migrations)
    ),
    'rows', jsonb_object_agg(name, coalesce(rows, '[]'::jsonb)),
    'counts', jsonb_object_agg(name, coalesce(row_count, 0))
) FROM exported_tables LEFT JOIN raw_tables ON name = relation;
COMMIT;
