import { createHash, randomBytes } from "node:crypto";
import { isCodeVerifier, verifyPkce } from "./pkce";
import { callbackUrl, parseRedirectUri } from "./redirect-uri";
import type { ConnectParams } from "./request";
import type { CodeStore, KeyStore } from "./store";

export const CODE_TTL_MS = 60_000;
export const EXPIRY_CHOICES = [0, 30, 90] as const; // 0 = never

export interface ConnectUser {
  id: string;
  email: string;
}
export interface Identity {
  issuer: string;
  subject: string;
}
export interface Tenant {
  id: string;
  name: string;
  /** Personal tenants are preselected on the consent page. */
  kind?: "standard" | "personal";
}
export interface OwnedKeyRef {
  id: string;
  name: string;
}

/** Everything the flow touches outside this module, so tests can replace it. */
export interface ConnectDeps {
  codes: CodeStore;
  keys: KeyStore;
  now(): Date;
  /** The user's OIDC identity, or null for a credential-only account. */
  identity(userId: string): Promise<Identity | null>;
  /** The account behind a user id when it may still sign in (active, not banned). */
  activeUser(userId: string): Promise<ConnectUser | null>;
  /** The tenants the user may create keys in: their memberships. */
  tenants(identity: Identity): Promise<Tenant[]>;
  listOwnedKeys(tenantId: string, identity: Identity): Promise<OwnedKeyRef[]>;
  createOwnedKey(tenantId: string, identity: Identity, name: string, actor: string): Promise<{ id: string; secret: string }>;
  /** Delete a key; a key that is already gone is not an error. */
  deleteKey(keyId: string, actor: string): Promise<void>;
  audit(actor: string, action: string, entityId: string, detail: Record<string, unknown>): Promise<void>;
}

export const sha256Hex = (s: string) => createHash("sha256").update(s).digest("hex");

export const keyNameFor = (device: string) => `dsh-${device}`;

export interface ApproveInput extends ConnectParams {
  tenantId: string;
  expiryDays: number;
}

export type ApproveResult = { ok: true; redirectTo: string } | { ok: false; error: "tenant_not_allowed" | "invalid_request" };

export async function approve(deps: ConnectDeps, user: ConnectUser, input: ApproveInput): Promise<ApproveResult> {
  if (!parseRedirectUri(input.redirectUri) || !(EXPIRY_CHOICES as readonly number[]).includes(input.expiryDays)) {
    return { ok: false, error: "invalid_request" };
  }
  const identity = await deps.identity(user.id);
  const tenant = identity ? (await deps.tenants(identity)).find((t) => t.id === input.tenantId) : undefined;
  if (!tenant) return { ok: false, error: "tenant_not_allowed" };

  const code = randomBytes(32).toString("base64url");
  const now = deps.now();
  const keyName = keyNameFor(input.device);
  await deps.codes.insert({
    codeHash: sha256Hex(code),
    codeChallenge: input.codeChallenge,
    redirectUri: input.redirectUri,
    userId: user.id,
    tenantId: tenant.id,
    keyName,
    expiryDays: input.expiryDays === 0 ? null : input.expiryDays,
    app: input.app,
    device: input.device,
    createdAt: now,
    expiresAt: new Date(now.getTime() + CODE_TTL_MS),
  });
  await deps.audit(user.email, "connect_approve", keyName, {
    user: user.email, tenant_id: tenant.id, tenant: tenant.name, key_name: keyName, app: input.app, device: input.device,
    expiry_days: input.expiryDays === 0 ? null : input.expiryDays,
  });
  return { ok: true, redirectTo: callbackUrl(input.redirectUri, { code }, input.state) };
}

export async function deny(deps: ConnectDeps, user: ConnectUser, params: ConnectParams): Promise<string> {
  await deps.audit(user.email, "connect_deny", keyNameFor(params.device), {
    user: user.email, key_name: keyNameFor(params.device), app: params.app, device: params.device,
  });
  return callbackUrl(params.redirectUri, { error: "access_denied" }, params.state);
}

export interface ExchangeOk {
  api_key: string;
  key_id: string;
  tenant: { id: string; name: string };
  expires_at: string | null;
}
export type ExchangeResult =
  | { status: 200; body: ExchangeOk }
  | { status: 400; body: { error: "invalid_grant" | "invalid_request" } }
  | { status: 500; body: { error: "server_error" } };

const bad = (error: "invalid_grant" | "invalid_request"): ExchangeResult => ({ status: 400, body: { error } });

export async function exchange(deps: ConnectDeps, body: unknown): Promise<ExchangeResult> {
  const b = (body && typeof body === "object" ? body : {}) as Record<string, unknown>;
  if (typeof b.code !== "string" || b.code.length === 0 || b.code.length > 256) return bad("invalid_request");
  if (!isCodeVerifier(b.code_verifier) || typeof b.redirect_uri !== "string") return bad("invalid_request");

  // Marked used before anything else is checked or minted: a replayed code, a
  // wrong verifier and a concurrent second exchange all find it gone.
  const now = deps.now();
  const row = await deps.codes.redeem(sha256Hex(b.code), now);
  if (!row) return bad("invalid_grant");
  if (row.redirectUri !== b.redirect_uri || !verifyPkce(b.code_verifier, row.codeChallenge)) return bad("invalid_grant");

  const user = await deps.activeUser(row.userId);
  const identity = user ? await deps.identity(row.userId) : null;
  if (!user || !identity) return bad("invalid_grant");
  const tenant = (await deps.tenants(identity)).find((t) => t.id === row.tenantId);
  if (!tenant) return bad("invalid_grant");

  try {
    // One key per device and tenant: the earlier key of this name goes first.
    for (const old of (await deps.listOwnedKeys(tenant.id, identity)).filter((k) => k.name === row.keyName)) {
      await deps.deleteKey(old.id, user.email);
      await deps.keys.remove(old.id);
    }
    const created = await deps.createOwnedKey(tenant.id, identity, row.keyName, user.email);
    const expiresAt = row.expiryDays ? new Date(now.getTime() + row.expiryDays * 86_400_000) : null;
    await deps.keys.insert({
      keyId: created.id,
      keyHash: sha256Hex(created.secret),
      userId: user.id,
      tenantId: tenant.id,
      keyName: row.keyName,
      expiresAt,
    });
    await deps.audit(user.email, "connect_exchange", created.id, {
      user: user.email, tenant_id: tenant.id, tenant: tenant.name, key_name: row.keyName, app: row.app, device: row.device,
      expires_at: expiresAt?.toISOString() ?? null,
    });
    return {
      status: 200,
      body: { api_key: created.secret, key_id: created.id, tenant: { id: tenant.id, name: tenant.name }, expires_at: expiresAt?.toISOString() ?? null },
    };
  } catch {
    return { status: 500, body: { error: "server_error" } };
  }
}

/** Delete the connect-issued key behind a bearer secret. Unknown keys are a no-op. */
export async function revoke(deps: ConnectDeps, apiKey: string): Promise<void> {
  const rec = await deps.keys.findByHash(sha256Hex(apiKey));
  if (!rec) return;
  const user = await deps.activeUser(rec.userId).catch(() => null);
  const actor = user?.email ?? rec.userId;
  await deps.deleteKey(rec.keyId, actor);
  await deps.keys.remove(rec.keyId);
  await deps.audit(actor, "connect_revoke", rec.keyId, {
    user: actor, tenant_id: rec.tenantId, key_name: rec.keyName,
  });
}

/** Delete the keys whose expiry has passed. */
export async function sweepExpired(deps: ConnectDeps): Promise<number> {
  const due = await deps.keys.expired(deps.now());
  for (const rec of due) {
    await deps.deleteKey(rec.keyId, "connect-expiry");
    await deps.keys.remove(rec.keyId);
  }
  return due.length;
}

export function bearerToken(header: string | null): string | null {
  const m = /^Bearer\s+(\S+)$/i.exec(header ?? "");
  return m ? m[1]! : null;
}
