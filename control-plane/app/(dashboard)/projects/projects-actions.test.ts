import { afterEach, describe, expect, it, vi } from "vitest";
afterEach(() => vi.resetModules());

const TID = "7b0f5c1e-1d2a-4c3b-8e4f-5a6b7c8d9e0f";

function setup(opts: { admin?: boolean; ids?: unknown[]; kindFails?: boolean; memberFails?: boolean } = {}) {
  const requireAdmin = vi.fn(async () => {
    if (opts.admin === false) throw new Error("Unauthorized");
    return { id: "a", email: "admin@example.com", role: "admin", status: "active", tenantId: null };
  });
  const obleth = {
    createTenant: vi.fn().mockResolvedValue({ id: TID, name: "p-alpha" }),
    deleteTenant: vi.fn().mockResolvedValue(undefined),
  };
  const accounts = {
    identitiesByEmail: vi.fn().mockResolvedValue(opts.ids ?? [{ issuer: "iss", subject: "s1", email: "m@x", personal_tenant_id: null }]),
    setTenantKind: opts.kindFails ? vi.fn().mockRejectedValue(new Error("kind boom")) : vi.fn().mockResolvedValue(undefined),
    upsertMember: opts.memberFails ? vi.fn().mockRejectedValue(new Error("member boom")) : vi.fn().mockResolvedValue({}),
    removeMember: vi.fn().mockResolvedValue(undefined),
  };
  vi.doMock("@/lib/auth/roles", () => ({ requireAdmin }));
  vi.doMock("@/lib/obleth", () => ({ obleth }));
  vi.doMock("@/lib/obleth-accounts", () => ({ accounts }));
  vi.doMock("@/lib/db", () => ({ getDb: () => ({ query: async () => ({ rows: [] }) }) }));
  vi.doMock("next/cache", () => ({ revalidatePath: vi.fn() }));
  return { obleth, accounts };
}

function form(fields: Record<string, string>): FormData {
  const fd = new FormData();
  for (const [k, v] of Object.entries(fields)) fd.set(k, v);
  return fd;
}

describe("projects actions", () => {
  it("createProject creates tenant, kind, manager", async () => {
    const { obleth, accounts } = setup();
    const { createProject } = await import("./projects-actions");
    expect(await createProject(form({ name: "p-alpha", manager: "M@x.io" }))).toEqual({ ok: true });
    expect(obleth.createTenant).toHaveBeenCalledWith({ name: "p-alpha", fairshare_group: "projects" }, { auditActor: "admin@example.com" });
    expect(accounts.identitiesByEmail).toHaveBeenCalledWith("m@x.io");
    expect(accounts.setTenantKind).toHaveBeenCalledWith(TID, "project", { auditActor: "admin@example.com" });
    expect(accounts.upsertMember).toHaveBeenCalledWith(TID, expect.objectContaining({ role: "manager", subject: "s1" }), expect.anything());
  });

  it("requires_admin on every action", async () => {
    const { obleth, accounts } = setup({ admin: false });
    const m = await import("./projects-actions");
    await expect(m.createProject(form({ name: "p", manager: "m@x.io" }))).rejects.toThrow("Unauthorized");
    await expect(m.addProjectMember(form({}))).rejects.toThrow("Unauthorized");
    await expect(m.removeProjectMember(form({}))).rejects.toThrow("Unauthorized");
    expect(obleth.createTenant).not.toHaveBeenCalled();
    expect(accounts.upsertMember).not.toHaveBeenCalled();
    expect(accounts.removeMember).not.toHaveBeenCalled();
  });

  it("create_project_rollback when setTenantKind fails", async () => {
    const { obleth } = setup({ kindFails: true });
    const { createProject } = await import("./projects-actions");
    expect(await createProject(form({ name: "p-alpha", manager: "m@x.io" }))).toEqual({ ok: false, error: "kind boom" });
    expect(obleth.deleteTenant).toHaveBeenCalledWith(TID, { auditActor: "admin@example.com" });
  });

  it("create_project_rollback when manager upsert fails", async () => {
    const { obleth } = setup({ memberFails: true });
    const { createProject } = await import("./projects-actions");
    expect((await createProject(form({ name: "p-alpha", manager: "m@x.io" }))).ok).toBe(false);
    expect(obleth.deleteTenant).toHaveBeenCalledWith(TID, expect.anything());
  });

  it("unknown manager email creates nothing", async () => {
    const { obleth } = setup({ ids: [] });
    const { createProject } = await import("./projects-actions");
    expect(await createProject(form({ name: "p", manager: "m@x.io" }))).toEqual({ ok: false, error: "User must sign in once first" });
    expect(obleth.createTenant).not.toHaveBeenCalled();
  });

  it("addProjectMember by email; unknown email refused", async () => {
    const a = setup();
    const m = await import("./projects-actions");
    expect(await m.addProjectMember(form({ tenantId: TID, email: "B@x.io", role: "member" }))).toEqual({ ok: true });
    expect(a.accounts.upsertMember).toHaveBeenCalledWith(TID, { issuer: "iss", subject: "s1", email: "b@x.io", role: "member" }, { auditActor: "admin@example.com" });
    a.accounts.identitiesByEmail.mockResolvedValue([]);
    expect(await m.addProjectMember(form({ tenantId: TID, email: "c@x.io", role: "member" }))).toEqual({ ok: false, error: "User must sign in once first" });
  });

  it("removeProjectMember", async () => {
    const { accounts } = setup();
    const { removeProjectMember } = await import("./projects-actions");
    expect(await removeProjectMember(form({ tenantId: TID, issuer: "iss", subject: "s1" }))).toEqual({ ok: true });
    expect(accounts.removeMember).toHaveBeenCalledWith(TID, "iss", "s1", { auditActor: "admin@example.com" });
  });

  it("multiple identities for one email creates nothing", async () => {
    const two = [
      { issuer: "i1", subject: "s1", email: "m@x", personal_tenant_id: null },
      { issuer: "i2", subject: "s2", email: "m@x", personal_tenant_id: null },
    ];
    const { obleth, accounts } = setup({ ids: two });
    const m = await import("./projects-actions");
    const err = { ok: false, error: "Email matches multiple identities" };
    expect(await m.createProject(form({ name: "p-alpha", manager: "m@x.io" }))).toEqual(err);
    expect(await m.addProjectMember(form({ tenantId: TID, email: "m@x.io", role: "member" }))).toEqual(err);
    expect(obleth.createTenant).not.toHaveBeenCalled();
    expect(accounts.upsertMember).not.toHaveBeenCalled();
  });

  it("failed rollback reports the orphaned tenant", async () => {
    const { obleth } = setup({ kindFails: true });
    obleth.deleteTenant.mockRejectedValue(new Error("delete boom"));
    const spy = vi.spyOn(console, "error").mockImplementation(() => undefined);
    const { createProject } = await import("./projects-actions");
    const r = await createProject(form({ name: "p-alpha", manager: "m@x.io" }));
    expect(r).toEqual({ ok: false, error: `kind boom; rollback failed, tenant ${TID} left behind` });
    expect(spy).toHaveBeenCalled();
    spy.mockRestore();
  });
});
