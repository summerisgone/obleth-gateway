-- Idempotent: safe to re-run.
-- How a model takes reasoning effort: one JSON object,
-- {"mode", "default_level", "levels"}, read tolerantly like `lifecycle`, so a
-- field added later needs no migration. Every pre-migration row reads as
-- pass-through (requests reach the model as the caller sent them).
alter table models add column if not exists reasoning jsonb not null default '{}'::jsonb;

-- One name for several models, with the effort level choosing which answers
-- (e.g. an instruct model at `none`, its thinking sibling at the rest).
-- `members` is {"none", "low", "medium", "high"}, each a models.model_name.
-- No foreign keys: a member is named, not linked, so renaming or deleting a
-- model is checked by the API, and a family whose member is gone answers 404
-- for that level rather than vanishing.
create table if not exists model_families (
    id            uuid primary key,
    name          text not null unique,
    description   text not null default '',
    default_level text not null default 'none',
    members       jsonb not null default '{}'::jsonb,
    created_at    timestamptz not null default now(),
    updated_at    timestamptz not null default now()
);
