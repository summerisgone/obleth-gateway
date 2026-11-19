import { getDb } from "@/lib/db";
import { userIdentity } from "@/lib/auth/personal";
import { obleth, OblethApiError } from "@/lib/obleth";
import { accounts } from "@/lib/obleth-accounts";
import { pgCodeStore, pgKeyStore } from "./store";
import type { ConnectDeps } from "./service";

const BASE = () => process.env.OBLETH_ADMIN_BASE_URL ?? "http://localhost:9180";

async function recordAudit(actor: string, action: string, entityId: string, detail: Record<string, unknown>): Promise<void> {
  try {
    const res = await fetch(`${BASE()}/api/v1/audit/events`, {
      method: "POST",
      cache: "no-store",
      headers: {
        Authorization: `Bearer ${process.env.OBLETH_ADMIN_TOKEN ?? ""}`,
        "Content-Type": "application/json",
        "X-Obleth-Audit-Actor": actor.replace(/[\r\n]+/g, " "),
      },
      body: JSON.stringify({ action, entity_type: "connect", entity_id: entityId, detail }),
    });
    if (!res.ok) console.error(`connect audit ${action} failed with status ${res.status}`);
  } catch (e) {
    // Never blocks the flow and never carries a secret: status or error name only.
    console.error(`connect audit ${action} failed`, e instanceof Error ? e.name : "error");
  }
}

export function connectDeps(): ConnectDeps {
  const db = getDb();
  return {
    codes: pgCodeStore(db),
    keys: pgKeyStore(db),
    now: () => new Date(),
    identity: (userId) => userIdentity(userId),
    async activeUser(userId) {
      const { rows } = await db.query(
        `select id, email from "user"
          where id = $1 and status = 'active'
            and (not coalesce(banned, false) or ("banExpires" is not null and "banExpires" < now()))`,
        [userId],
      );
      return rows[0] ? { id: rows[0].id, email: rows[0].email } : null;
    },
    async tenants(identity) {
      const ms = await accounts.memberships(identity.issuer, identity.subject);
      return ms.map((m) => ({ id: m.tenant_id, name: m.tenant_name, kind: m.kind }));
    },
    async listOwnedKeys(tenantId, identity) {
      const keys = await accounts.listOwnedKeys(tenantId, identity);
      return keys.map((k) => ({ id: k.id, name: k.name }));
    },
    async createOwnedKey(tenantId, identity, name, actor) {
      const created = await accounts.createOwnedKey(tenantId, { ...identity, name }, { auditActor: actor });
      return { id: created.key.id, secret: created.secret };
    },
    async deleteKey(keyId, actor) {
      try {
        await obleth.deleteKey(keyId, { auditActor: actor });
      } catch (e) {
        if (!(e instanceof OblethApiError && e.status === 404)) throw e;
      }
    },
    audit: recordAudit,
  };
}
