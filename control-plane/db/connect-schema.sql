-- Browser key issuance ("connect", docs/adr/0001-browser-key-issuance.md).
-- Idempotent: applied with auth-schema.sql at control-plane startup.

-- One-time authorization codes. Only the SHA-256 of a code is stored.
create table if not exists connect_codes (
  code_hash text primary key,
  code_challenge text not null,
  redirect_uri text not null,
  user_id text not null references "user"(id) on delete cascade,
  tenant_id uuid not null references tenants(id) on delete cascade,
  key_name text not null,
  expiry_days integer,
  app text not null default '',
  device text not null default '',
  created_at timestamptz not null default now(),
  expires_at timestamptz not null,
  used_at timestamptz
);
create index if not exists connect_codes_created_at_idx on connect_codes (created_at);

-- Keys minted by connect: revoke finds a key by the hash of its secret, and the
-- control plane deletes a key when its expiry passes (the gateway has no expiry).
create table if not exists connect_keys (
  key_id uuid primary key,
  key_hash text not null unique,
  user_id text not null references "user"(id) on delete cascade,
  tenant_id uuid not null references tenants(id) on delete cascade,
  key_name text not null,
  created_at timestamptz not null default now(),
  expires_at timestamptz
);
create index if not exists connect_keys_expires_at_idx on connect_keys (expires_at) where expires_at is not null;
