-- Idempotent: safe to re-run.
-- Top-level request body fields a model's backend cannot handle, as a JSON
-- array of names. A request carrying one is refused with 400 before it is
-- sent. Every pre-migration row reads as refusing nothing.
alter table models add column if not exists rejected_request_fields jsonb not null default '[]'::jsonb;
