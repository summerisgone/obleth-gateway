import { ProjectsList } from "@/components/projects/projects-list";
import { requireAdmin } from "@/lib/auth/roles";
import { listUsers } from "@/lib/auth/users";
import { accounts, type Member, type ProjectSummary } from "@/lib/obleth-accounts";
import { safe } from "@/lib/safe";

export const dynamic = "force-dynamic";

export default async function ProjectsPage() {
  await requireAdmin();
  const [projects, users] = await Promise.all([safe(accounts.listProjects(), [] as ProjectSummary[]), listUsers()]);
  const lists = await Promise.all(projects.map((p) => safe(accounts.listMembers(p.tenant_id), [] as Member[])));
  const members = Object.fromEntries(projects.map((p, i) => [p.tenant_id, lists[i]]));
  const emails = [...new Set(users.map((u) => u.email.toLowerCase()))].sort();
  return <ProjectsList projects={projects} members={members} users={emails} />;
}
