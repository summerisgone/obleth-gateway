"use client";

import { useState, useTransition } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import { Plus, Search } from "lucide-react";
import { addProjectMember, createProject, removeProjectMember } from "@/app/(dashboard)/projects/projects-actions";
import { Notice, Sheet } from "@/components/models/ui";
import { Pill } from "@/components/overview/ui";
import { Button } from "@/components/ui/button";
import { useConfirm } from "@/components/ui/confirm-dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Select } from "@/components/ui/select";
import { tenantHref } from "@/lib/access-model";
import type { Member, ProjectSummary } from "@/lib/obleth-accounts";
import { cn } from "@/lib/utils";

const COLS = "grid-cols-[minmax(0,1.2fr)_minmax(0,1.6fr)_96px_96px]";
const ROLES = [{ value: "member", label: "member" }, { value: "manager", label: "manager" }];

export function ProjectsList({ projects, members, users }: { projects: ProjectSummary[]; members: Record<string, Member[]>; users: string[] }) {
  const router = useRouter();
  const [query, setQuery] = useState("");
  const [open, setOpen] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const q = query.trim().toLowerCase();
  const shown = projects.filter((p) => !q || p.name.toLowerCase().includes(q) || (members[p.tenant_id] ?? []).some((m) => (m.email ?? m.subject).toLowerCase().includes(q)));
  const openProject = projects.find((p) => p.tenant_id === open) ?? null;
  const total = projects.reduce((n, p) => n + p.members, 0);

  return (
    <div className="mx-auto flex max-w-[1600px] flex-col gap-[18px]">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div className="flex min-w-0 flex-col gap-1.5">
          <h1 className="text-[26px] font-semibold tracking-tight">Projects</h1>
          <p className="text-[13px] text-secondary-foreground">{projects.length} project{projects.length === 1 ? "" : "s"} · {total} membership{total === 1 ? "" : "s"}</p>
        </div>
        <Button type="button" size="sm" className="h-9" onClick={() => setAdding(true)}><Plus className="h-4 w-4" />New project</Button>
      </div>

      {notice && <Notice onDismiss={() => setNotice(null)}>{notice}</Notice>}

      <label className="flex h-9 min-w-[240px] max-w-[380px] items-center gap-2 rounded-lg border border-border bg-background px-3 text-[13px] focus-within:border-muted-foreground">
        <Search className="h-4 w-4 shrink-0 text-muted-foreground" aria-hidden />
        <input value={query} onChange={(e) => setQuery(e.target.value)} aria-label="Search projects" placeholder="Project or member email" className="min-w-0 flex-1 bg-transparent outline-none placeholder:text-muted-foreground" />
      </label>

      <section aria-label="Projects" className="overflow-hidden rounded-xl border border-border bg-card">
        <div className="overflow-x-auto">
          <div className="min-w-[720px]">
            <div className={cn("grid items-center gap-3.5 px-[18px] py-2.5 text-[11px] font-semibold uppercase tracking-[0.07em] text-muted-foreground", COLS)}>
              <span>Project</span><span>Managers</span><span className="text-right">Members</span><span className="text-right">Tenant</span>
            </div>
            {shown.length === 0 && <p className="border-t border-border px-[18px] py-8 text-center text-[13px] text-muted-foreground">{projects.length ? "No project matches this search." : "No projects yet."}</p>}
            {shown.map((p) => {
              const managers = (members[p.tenant_id] ?? []).filter((m) => m.role !== "member").map((m) => m.email ?? m.subject);
              return (
                <div
                  key={p.tenant_id}
                  onClick={(e) => {
                    if (e.target instanceof Element && e.target.closest("a, button")) return;
                    setOpen(p.tenant_id);
                  }}
                  className={cn("grid min-h-[52px] cursor-pointer items-center gap-3.5 border-t border-border px-[18px] py-2 text-[13px] hover:bg-muted/30", COLS, open === p.tenant_id && "bg-muted/40")}
                >
                  <button type="button" aria-label={`Open ${p.name}`} onClick={() => setOpen(p.tenant_id)} className="truncate text-left font-medium text-foreground">{p.name}</button>
                  <span className="truncate text-secondary-foreground">{managers.length ? managers.join(", ") : "—"}</span>
                  <span className="text-right font-mono text-[12px]">{p.members}</span>
                  <Link href={tenantHref(p)} className="text-right text-[12px] underline decoration-muted-foreground/40 underline-offset-[3px] hover:decoration-foreground">Settings</Link>
                </div>
              );
            })}
          </div>
        </div>
        <div className="border-t border-border px-[18px] py-3 text-[12.5px] text-muted-foreground">
          {shown.length === projects.length ? `${projects.length} projects` : `${shown.length} of ${projects.length} projects`} · click one to manage its members
        </div>
      </section>

      {openProject && (
        <MembersPanel
          key={openProject.tenant_id}
          project={openProject}
          members={members[openProject.tenant_id] ?? []}
          users={users}
          onClose={() => setOpen(null)}
          onChanged={() => router.refresh()}
        />
      )}
      {adding && (
        <NewProjectSheet
          onClose={() => setAdding(false)}
          onCreated={(name) => { setNotice(`Project ${name} created.`); setAdding(false); router.refresh(); }}
        />
      )}
    </div>
  );
}

function NewProjectSheet({ onClose, onCreated }: { onClose: () => void; onCreated: (name: string) => void }) {
  const [pending, start] = useTransition();
  const [error, setError] = useState<string | null>(null);
  return (
    <Sheet open onClose={onClose} title="New project" description="A project is a tenant with members. Its manager can add people and keys." width="w-[min(520px,100vw)]">
      <form
        className="flex flex-col gap-4 px-6 pb-6"
        onSubmit={(e) => {
          e.preventDefault();
          const fd = new FormData(e.currentTarget);
          start(async () => {
            setError(null);
            const res = await createProject(fd);
            if (res.ok) onCreated(String(fd.get("name")));
            else setError(res.error);
          });
        }}
      >
        <fieldset disabled={pending} className="grid gap-x-4 gap-y-3 md:grid-cols-2">
          <div className="space-y-1.5">
            <Label htmlFor="project-name">Name</Label>
            <Input id="project-name" name="name" required maxLength={63} placeholder="star" />
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="project-manager">Manager email</Label>
            <Input id="project-manager" name="manager" type="email" required placeholder="name@ai.local" />
          </div>
        </fieldset>
        {error && <div role="alert" className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">{error}</div>}
        <div className="flex justify-end">
          <Button type="submit" disabled={pending}><Plus className="h-3.5 w-3.5" aria-hidden />Create project</Button>
        </div>
      </form>
    </Sheet>
  );
}

function MembersPanel({ project, members, users, onClose, onChanged }: { project: ProjectSummary; members: Member[]; users: string[]; onClose: () => void; onChanged: () => void }) {
  const { confirm, confirmElement } = useConfirm();
  const [pending, start] = useTransition();
  const [error, setError] = useState<string | null>(null);
  const [email, setEmail] = useState("");
  const taken = new Set(members.map((m) => m.email?.toLowerCase()));
  const candidates = users.filter((u) => !taken.has(u)).map((u) => ({ value: u, label: u }));

  function remove(m: Member) {
    start(async () => {
      const who = m.email ?? m.subject;
      const ok = await confirm({ title: `Remove ${who} from ${project.name}?`, description: "Their keys in this project stop working.", confirmLabel: "Remove" });
      if (!ok) return;
      const fd = new FormData();
      fd.set("tenantId", project.tenant_id);
      fd.set("issuer", m.issuer);
      fd.set("subject", m.subject);
      const res = await removeProjectMember(fd);
      if (res.ok) { setError(null); onChanged(); } else setError(res.error);
    });
  }

  return (
    <>
      {confirmElement}
      <Sheet
        open
        onClose={onClose}
        title={project.name}
        width="w-[min(620px,100vw)]"
        description={
          <span className="flex flex-wrap items-center gap-2">
            <Pill>project</Pill>
            <span>{members.length} member{members.length === 1 ? "" : "s"} · <Link href={tenantHref(project)} className="text-secondary-foreground underline underline-offset-2 hover:text-foreground">tenant settings</Link></span>
          </span>
        }
      >
        <div className="flex flex-col gap-5 px-6 pb-6">
          <form
            className="flex flex-col gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              const form = e.currentTarget;
              const fd = new FormData(form);
              start(async () => {
                const res = await addProjectMember(fd);
                if (res.ok) { setError(null); setEmail(""); form.reset(); onChanged(); } else setError(res.error);
              });
            }}
          >
            <span className="text-[11px] font-semibold uppercase tracking-[0.07em] text-muted-foreground">Add a member</span>
            <fieldset disabled={pending} className="flex flex-wrap items-center gap-2">
              <input type="hidden" name="tenantId" value={project.tenant_id} />
              <Select name="email" required searchable value={email} onValueChange={setEmail} aria-label="Member email" placeholder={candidates.length ? "Choose a user" : "Everyone is already a member"} searchPlaceholder="Find by email" className="min-w-[200px] flex-1" options={candidates} />
              <Select name="role" defaultValue="member" aria-label="Role" className="w-32" options={ROLES} />
              <Button type="submit" size="sm" className="h-9" disabled={!email}><Plus className="h-3.5 w-3.5" aria-hidden />Add</Button>
            </fieldset>
          </form>

          {error && <div role="alert" className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">{error}</div>}

          <section aria-label="Members" className="overflow-hidden rounded-lg border border-border">
            <div className="grid grid-cols-[minmax(0,1fr)_88px_88px] items-center gap-3 px-4 py-2 text-[11px] font-semibold uppercase tracking-[0.07em] text-muted-foreground">
              <span>Member</span><span>Role</span><span />
            </div>
            {members.length === 0 && <p className="border-t border-border px-4 py-6 text-center text-[13px] text-muted-foreground">No members yet.</p>}
            {members.map((m) => (
              <div key={`${m.issuer}|${m.subject}`} className="grid min-h-[44px] grid-cols-[minmax(0,1fr)_88px_88px] items-center gap-3 border-t border-border px-4 py-1.5 text-[13px]">
                <span className="truncate">{m.email ?? <span className="font-mono text-[12px]">{m.subject}</span>}</span>
                <span><Pill>{m.role}</Pill></span>
                <span className="text-right">
                  {m.role !== "owner" && <Button type="button" variant="ghost" size="sm" disabled={pending} onClick={() => remove(m)}>Remove</Button>}
                </span>
              </div>
            ))}
          </section>
        </div>
      </Sheet>
    </>
  );
}
