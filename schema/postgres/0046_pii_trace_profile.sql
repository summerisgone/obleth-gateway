-- Per-tenant PII profile for GenAI trace scrubbing (null = inherit the
-- gateway default). Idempotent: safe to re-run.

alter table tenants add column if not exists pii_trace_profile text;
