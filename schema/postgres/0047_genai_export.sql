-- GenAI OTLP export: per-tenant opt-in, per-key opt-out (null = inherit),
-- and the upstream provider name on a model. Idempotent: safe to re-run.

alter table tenants add column if not exists genai_export boolean not null default false;
alter table api_keys add column if not exists genai_export boolean null;
alter table models add column if not exists provider text not null default '';
