"use server";

import { revalidatePath } from "next/cache";
import { z } from "zod";
import { requireAdmin } from "@/lib/auth/roles";
import { accounts } from "@/lib/obleth-accounts";
import { obleth } from "@/lib/obleth";
import { resolveIdentityByEmail } from "@/lib/identity-lookup";
import type { ActionResult } from "@/app/actions";

const createSchema = z.object({
  name: z.string().trim().min(1).max(63),
  manager: z.string().trim().toLowerCase().email(),
});
const memberSchema = z.object({
  tenantId: z.string().uuid(),
  email: z.string().trim().toLowerCase().email(),
  role: z.enum(["member", "manager"]),
});
const removeSchema = z.object({
  tenantId: z.string().uuid(),
  issuer: z.string().min(1),
  subject: z.string().min(1),
});

function message(e: unknown): string {
  return e instanceof Error ? e.message : "Request failed";
}

export async function createProject(formData: FormData): Promise<ActionResult> {
  const admin = await requireAdmin();
  const opts = { auditActor: admin.email };
  let p: z.infer<typeof createSchema>;
  try {
    p = createSchema.parse({ name: formData.get("name"), manager: formData.get("manager") });
  } catch (e) {
    return { ok: false, error: message(e) };
  }
  let id: string | null = null;
  try {
    const found = await resolveIdentityByEmail(p.manager);
    if (!found.ok) return found;
    const ident = found.identity;
    const t = await obleth.createTenant({ name: p.name, fairshare_group: "projects" }, opts);
    id = t.id;
    await accounts.setTenantKind(id, "project", opts);
    await accounts.upsertMember(id, { issuer: ident.issuer, subject: ident.subject, email: p.manager, role: "manager" }, opts);
  } catch (e) {
    let error = message(e);
    if (id) {
      try {
        await obleth.deleteTenant(id, opts);
      } catch (re) {
        console.error(`createProject rollback failed, tenant ${id} left behind`, re);
        error += `; rollback failed, tenant ${id} left behind`;
      }
    }
    return { ok: false, error };
  }
  revalidatePath("/projects");
  return { ok: true };
}

export async function addProjectMember(formData: FormData): Promise<ActionResult> {
  const admin = await requireAdmin();
  try {
    const p = memberSchema.parse({
      tenantId: formData.get("tenantId"),
      email: formData.get("email"),
      role: formData.get("role") ?? "member",
    });
    const found = await resolveIdentityByEmail(p.email);
    if (!found.ok) return found;
    const ident = found.identity;
    await accounts.upsertMember(
      p.tenantId,
      { issuer: ident.issuer, subject: ident.subject, email: p.email, role: p.role },
      { auditActor: admin.email },
    );
    revalidatePath("/projects");
    return { ok: true };
  } catch (e) {
    return { ok: false, error: message(e) };
  }
}

export async function removeProjectMember(formData: FormData): Promise<ActionResult> {
  const admin = await requireAdmin();
  try {
    const p = removeSchema.parse({
      tenantId: formData.get("tenantId"),
      issuer: formData.get("issuer"),
      subject: formData.get("subject"),
    });
    await accounts.removeMember(p.tenantId, p.issuer, p.subject, { auditActor: admin.email });
    revalidatePath("/projects");
    return { ok: true };
  } catch (e) {
    return { ok: false, error: message(e) };
  }
}
