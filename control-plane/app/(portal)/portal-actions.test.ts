import { afterEach, expect, it, vi } from "vitest";
afterEach(() => vi.resetModules());

type Role = "member" | "manager";

function setup(role: Role, opts: { own?: string[]; all?: string[] } = {}) {
  const own = opts.own ?? ["k-mine"];
  const all = opts.all ?? ["k-mine", "k-other"];
  const ctx = {
    identity: { issuer: "iss", subject: "s1" },
    tenants: [],
    current: { id: "p1", name: "p-alpha", kind: "project", role },
  };
  const listOwnedKeys = vi.fn(async (_t: string, owner?: unknown) =>
    (owner ? own : all).map((id) => ({ id })),
  );
  const createOwnedKey = vi.fn().mockResolvedValue({ key: { id: "k-new" }, secret: "sk-secret" });
  const deleteKey = vi.fn().mockResolvedValue(undefined);
  const setKeyDisabled = vi.fn().mockResolvedValue(undefined);
  const set = vi.fn();
  const loadPortalContext = vi.fn(async (_u: unknown, requested?: string) => ({
    ...ctx,
    tenants: [{ id: "p1" }, { id: "u1" }],
    current: requested === "u1" ? { ...ctx.current, id: "u1" } : ctx.current,
  }));
  vi.doMock("@/lib/auth/roles", () => ({
    requireUser: async () => ({ id: "user-A", email: "a@example.com", role: "user", status: "active", tenantId: "legacy" }),
  }));
  vi.doMock("@/lib/portal-context", () => ({
    PORTAL_TENANT_COOKIE: "portal_tenant",
    currentPortalContext: async () => ctx,
    loadPortalContext,
  }));
  vi.doMock("@/lib/obleth-accounts", () => ({ accounts: { listOwnedKeys, createOwnedKey } }));
  vi.doMock("@/lib/obleth", () => ({ obleth: { deleteKey, setKeyDisabled } }));
  vi.doMock("next/cache", () => ({ revalidatePath: vi.fn() }));
  vi.doMock("next/headers", () => ({ cookies: async () => ({ set }) }));
  return { listOwnedKeys, createOwnedKey, deleteKey, setKeyDisabled, set };
}

const fd = (o: Record<string, string>) => {
  const f = new FormData();
  for (const [k, v] of Object.entries(o)) f.set(k, v);
  return f;
};

it("createPortalKey uses current tenant and session identity, ignoring form tenant", async () => {
  const m = setup("member");
  const { createPortalKey } = await import("./portal-actions");
  const res = await createPortalKey(fd({ name: "My Key", tenantId: "foreign" }));
  expect(res).toEqual({ ok: true, secret: "sk-secret" });
  expect(m.createOwnedKey).toHaveBeenCalledWith(
    "p1",
    { issuer: "iss", subject: "s1", name: "My Key" },
    { auditActor: "a@example.com" },
  );
});

it("createPortalKey rejects a blank name", async () => {
  const m = setup("member");
  const { createPortalKey } = await import("./portal-actions");
  expect((await createPortalKey(fd({ name: "  " }))).ok).toBe(false);
  expect(m.createOwnedKey).not.toHaveBeenCalled();
});

it("member cannot delete another member's key", async () => {
  const m = setup("member");
  const { deletePortalKey } = await import("./portal-actions");
  expect(await deletePortalKey(fd({ id: "k-other" }))).toEqual({ ok: false, error: "Key not found" });
  expect(m.deleteKey).not.toHaveBeenCalled();
});

it("member deletes own key", async () => {
  const m = setup("member");
  const { deletePortalKey } = await import("./portal-actions");
  expect((await deletePortalKey(fd({ id: "k-mine" }))).ok).toBe(true);
  expect(m.deleteKey).toHaveBeenCalledWith("k-mine", { auditActor: "a@example.com" });
});

it("manager deletes another member's key but not a key outside the tenant", async () => {
  const m = setup("manager");
  const { deletePortalKey } = await import("./portal-actions");
  expect((await deletePortalKey(fd({ id: "k-other" }))).ok).toBe(true);
  expect(await deletePortalKey(fd({ id: "k-foreign" }))).toEqual({ ok: false, error: "Key not found" });
  expect(m.deleteKey).toHaveBeenCalledTimes(1);
});

it("member cannot disable another member's key; manager can", async () => {
  let m = setup("member");
  let { disablePortalKey } = await import("./portal-actions");
  expect((await disablePortalKey(fd({ id: "k-other", disabled: "true" }))).ok).toBe(false);
  expect(m.setKeyDisabled).not.toHaveBeenCalled();
  vi.resetModules();
  m = setup("manager");
  ({ disablePortalKey } = await import("./portal-actions"));
  expect((await disablePortalKey(fd({ id: "k-other", disabled: "true" }))).ok).toBe(true);
  expect(m.setKeyDisabled).toHaveBeenCalledWith("k-other", true, { auditActor: "a@example.com" });
});

it("re-enable is still refused, even for a manager's own key", async () => {
  const m = setup("manager");
  const { disablePortalKey } = await import("./portal-actions");
  const res = await disablePortalKey(fd({ id: "k-mine", disabled: "false" }));
  expect(res).toEqual({ ok: false, error: "Contact an administrator to re-enable a key" });
  expect(m.setKeyDisabled).not.toHaveBeenCalled();
});

it("selectPortalTenant sets the cookie only for a membership", async () => {
  const m = setup("member");
  const { selectPortalTenant } = await import("./portal-actions");
  await selectPortalTenant(fd({ tenantId: "u1" }));
  expect(m.set).toHaveBeenCalledWith("portal_tenant", "u1", {
    httpOnly: true,
    sameSite: "lax",
    path: "/portal",
    secure: process.env.NODE_ENV === "production",
  });
  m.set.mockClear();
  await selectPortalTenant(fd({ tenantId: "forged" }));
  expect(m.set).not.toHaveBeenCalled();
});

it("createPortalKey rejects an over-long name", async () => {
  const m = setup("member");
  const { createPortalKey } = await import("./portal-actions");
  const r = await createPortalKey(fd({ name: "x".repeat(101) }));
  expect(r.ok).toBe(false);
  expect(m.createOwnedKey).not.toHaveBeenCalled();
});

it("cookie is Secure in production", async () => {
  vi.stubEnv("NODE_ENV", "production");
  const m = setup("member");
  const { selectPortalTenant } = await import("./portal-actions");
  await selectPortalTenant(fd({ tenantId: "u1" }));
  expect(m.set.mock.calls[0][2]).toMatchObject({ secure: true });
  vi.unstubAllEnvs();
});
