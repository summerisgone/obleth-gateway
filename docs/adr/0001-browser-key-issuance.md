# 0001: Browser-based key issuance ("connect") for desktop apps

## Status

Accepted on 2026-11-19. Format follows `docs/adr/` of obleth-stack (Status, Context, Decision, Alternatives, Consequences); this repo had no `docs/` before.

## Context

The DeepSeek Harness desktop app (DSH) needs an obleth API key. Today a user opens the portal Keys page, creates a key and pastes it into the app. We want the app to open the browser, the user approves, and the app receives a key without copy-paste.

Facts checked in this repo (branch `local_combined`):

- Portal sign-in is better-auth with generic OIDC (OpenBao). A new account is `pending` until an admin approves it or `OIDC_AUTO_APPROVE=true`; pending users are sent to `/awaiting-approval`. `proxy.ts` only checks that a session cookie exists; routes authorize on their own.
- A portal user belongs to several tenants (personal `u-<sub>` and projects). Membership comes from the gateway (`GET /identities/memberships`); `lib/portal-context.ts` builds the list. Every member, whatever the role, may create an own key in the tenant (`createPortalKey` -> `POST /tenants/:id/owned-keys`). That is the rule for "may create keys".
- Key creation, deletion and the owner filter (`GET /tenants/:id/owned-keys?issuer&subject`) are admin API routes called with `OBLETH_ADMIN_TOKEN`; the gateway writes `audit_log` rows for them with the actor from `X-Obleth-Audit-Actor`. Tenant budgets and the model allowlist are tenant level, so any key created through `owned-keys` gets those of its tenant.
- The gateway has no key expiry and no way to find a key by its secret. There is no admin route that writes an arbitrary audit entry.
- The control plane already has a Postgres pool (`lib/db.ts`) and applies idempotent SQL at start (`db/*.sql`, `instrumentation.ts`).

## Decision

OAuth 2.0 authorization code with PKCE (RFC 7636, S256 only) and a loopback redirect (RFC 8252). The result is an obleth tenant API key, not a token.

1. `GET /connect` (portal page, outside the `(portal)` group). Validates `redirect_uri` first: exactly `http://127.0.0.1|[::1]|localhost:<port>/obleth/callback`, no query, fragment or userinfo; invalid -> error page, never a redirect. Missing or invalid `state` or `code_challenge` (43-128 base64url chars, `code_challenge_method=S256`) -> redirect with `error=invalid_request`. Not signed in -> `/login?next=<this URL>`; pending or tenant-less -> `/awaiting-approval`. The consent page shows app, device, user email, a tenant select (the user's memberships, personal preselected), expiry (never, 30, 90 days), Approve and Deny.
2. `POST /connect/decision` (form post). CSRF: hidden token = HMAC-SHA256 over user id, redirect_uri, state, challenge, app, device and an expiry time, keyed by the auth secret, plus an Origin check. Everything is validated again server side; the tenant must be one of the user's memberships at that moment. Approve inserts a code row and answers 302 to `redirect_uri?code&state`; Deny answers 302 `?error=access_denied&state`.
3. `POST /api/connect/token` (JSON, no cookie). Redeems the code with one atomic `UPDATE ... WHERE used_at IS NULL AND expires_at > now() RETURNING` (a replay fails even under concurrency), then checks `redirect_uri` equality and `BASE64URL(SHA256(verifier)) == challenge`. A code is burned by any redemption attempt, right or wrong. Mints the key through `owned-keys` for the approving user's identity, name `dsh-<device>`; earlier keys with that name owned by the user in that tenant are deleted first. Answers 200 `{api_key, key_id, tenant, expires_at}`, 400 `invalid_grant` / `invalid_request`, 429.
4. `POST /api/connect/revoke` with `Authorization: Bearer <key>`: deletes that key if it was issued by connect; 204 also for unknown keys.
5. Storage, applied at start from `db/connect-schema.sql`:
   - `connect_codes`: SHA-256 of the code, `code_challenge`, `redirect_uri`, `user_id`, `tenant_id`, `key_name`, `expiry_days`, `app`, `device`, `created_at`, `expires_at` (60 s), `used_at`. Rows older than 1 hour are deleted on every write.
   - `connect_keys`: `key_id`, SHA-256 of the key secret, user, tenant, name, `expires_at`. Needed because the gateway cannot look a key up by secret (revoke) and has no expiry (see below).
6. Expiry: the control plane deletes a key at its `expires_at` through the admin API (a sweep every minute, started with the control plane). A key can live up to about a minute past its expiry.
7. Audit: a new admin route `POST /api/v1/audit/events` writes `audit_log` rows (actions limited to `connect_approve`, `connect_deny`, `connect_exchange`, `connect_revoke`). Detail holds user, tenant, key name, app, device; never the code, verifier or key. Key creation and deletion are audited by the gateway as before, with the user's email as actor.
8. Limits: in-memory fixed-window rate limit, 30 requests per minute per client address for `/api/connect/token` and `/api/connect/revoke` (the control plane runs one replica). The address is the rightmost `X-Forwarded-For` entry counted `CONNECT_TRUSTED_PROXY_HOPS` (default 1) from the right. No CORS headers; `proxy.ts` already rejects browser cross-site posts to `/api/*`. `/connect` is sent with `X-Frame-Options: DENY` and `frame-ancestors 'none'`, and its `form-action` also allows the validated loopback origin, because Chrome applies `form-action` to the redirect after the post. Neither the code, the verifier nor a key is logged.

## Alternatives considered

- Device authorization grant (RFC 8628): no loopback listener, but the user types a code; worse flow for a desktop app that can listen.
- Return the key in the redirect: puts a long-lived secret in a URL (history, logs). The code is single use and 60 s.
- Reuse portal key creation action: it needs the session cookie; the desktop host has none.
- Gateway-side expiry and key lookup: right long term, but a Rust and schema change to the hot path; deferred.
- A control-plane-only audit table: would not show in the dashboard Audit page.

## Consequences

- One new gateway admin route (small Rust change) and two control-plane tables.
- Key expiry is enforced by the control plane (the e2e test does not cover it; unit tests do); if it is down, keys outlive their expiry until it returns.
- Revoke works only for keys issued by connect; other keys are revoked in the portal.
- A stolen code is useless without the verifier, but anyone who can reach the token endpoint may burn a code (denial of one flow, the user retries).
- Multiple control-plane replicas would need a shared rate limiter.
- Users without an OIDC identity (break-glass credential accounts) cannot connect.
