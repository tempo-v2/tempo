-- The published bank is authoritative even if the predecessor stopped mid-Apply.
CREATE INDEX forecast_bank_difference ON prepared_queue(card_id)
 WHERE rank_a IS NOT rank_b OR scope_a IS NOT scope_b OR slot_a IS NOT slot_b;
UPDATE prepared_queue SET rank_b=rank_a,scope_b=scope_a,slot_b=slot_a
 WHERE (rank_a IS NOT rank_b OR scope_a IS NOT scope_b OR slot_a IS NOT slot_b)
 AND (SELECT bank FROM forecast_order_state WHERE singleton=1)=0;
CREATE INDEX forecast_peak_a ON prepared_queue(coalesce(rank_a,rank))
 WHERE kind='opening';
UPDATE prepared_queue SET rank_a=rank_b,scope_a=scope_b,slot_a=slot_b
 WHERE (rank_a IS NOT rank_b OR scope_a IS NOT scope_b OR slot_a IS NOT slot_b)
 AND (SELECT bank FROM forecast_order_state WHERE singleton=1)=1;
CREATE INDEX forecast_peak_b ON prepared_queue(coalesce(rank_b,rank))
 WHERE kind='opening';
