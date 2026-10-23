-- Idempotent: safe to re-run.
-- A model's own session affinity setting for the fairshare queue: one JSON
-- object, {"enabled", "hold_ms", "max_held_percent", "max_bypass_secs"},
-- each optional and read tolerantly like `reasoning`. Every pre-migration
-- row reads as `{}`: the model follows the gateway's switch and values.
alter table models add column if not exists session_affinity jsonb not null default '{}'::jsonb;
