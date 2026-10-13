import type { FairshareLiveView, TenantFairshareView } from "@/lib/obleth";

// Canonical starvation predicate shared by the overview and fairshare pages so
// both agree on which tenants count as waiting below their fair share.
export interface FairshareTenantSlots {
  queued: number;
  in_flight: number;
  expected_slots: number;
}

/**
 * Whether a tenant's row in one pool is waiting below its fair share: it has
 * requests queued while it holds fewer slots than its share of that pool.
 * Only meaningful inside one pool; see {@link belowShareInPools}.
 */
export function isWaitingBelowShare(t: FairshareTenantSlots): boolean {
  return t.queued > 0 && t.in_flight < Math.floor(t.expected_slots);
}

/**
 * Whose queues, tenants and fair shares a view holds: every gateway
 * (`cluster`), the answering one of several (`gateway`), or the only one
 * (`single`, where the two are the same).
 */
export type CountsScope = "cluster" | "gateway" | "single";

export function countsScope(view: FairshareLiveView | undefined): CountsScope {
  if (view?.scope === "cluster") return "cluster";
  return Math.max(view?.replicas ?? 1, 1) > 1 ? "gateway" : "single";
}

/** A tenant waiting below its fair share in one pool. */
export interface BelowShare {
  /** The pool's model, or null when the view has no per-pool detail. */
  model: string | null;
  tenant: TenantFairshareView;
}

/**
 * Every pool where a tenant is waiting below its fair share, judged inside
 * each pool: a tenant can hold more than its share in one pool and be short
 * in another, and its rows added up across pools would hide that. A view
 * without per-pool detail (an older gateway) falls back to the tenants
 * aggregated across pools.
 */
export function belowShareInPools(view: FairshareLiveView | undefined): BelowShare[] {
  if (!view) return [];
  if (!view.pools) return view.tenants.filter(isWaitingBelowShare).map((tenant) => ({ model: null, tenant }));
  return view.pools.flatMap((p) => p.tenants.filter(isWaitingBelowShare).map((tenant) => ({ model: p.model, tenant })));
}

/** Tenants waiting below their fair share in at least one pool. */
export function tenantsBelowShare(view: FairshareLiveView | undefined): Set<string> {
  return new Set(belowShareInPools(view).map((b) => b.tenant.tenant_id));
}

/** Tenants holding at least one slot above their fair share in some pool. */
export function tenantsAboveShare(view: FairshareLiveView | undefined): Set<string> {
  const above = (t: TenantFairshareView) => t.in_flight - t.expected_slots >= 1;
  const rows = view?.pools ? view.pools.flatMap((p) => p.tenants) : view?.tenants ?? [];
  return new Set(rows.filter(above).map((t) => t.tenant_id));
}
