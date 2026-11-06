import { redirect } from "next/navigation";
import type { Member } from "@/lib/obleth-accounts";
import { addMember, removeMember } from "@/app/(portal)/portal/members/members-actions";

// Form actions must return void; report the outcome back to this page in the query string.
function back(r: { ok: true } | { ok: false; error: string }): never {
  const q = new URLSearchParams(r.ok ? { ok: "Saved" } : { error: r.error });
  redirect(`/portal/members?${q}`);
}
async function addAction(fd: FormData) {
  "use server";
  back(await addMember(fd));
}
async function removeAction(fd: FormData) {
  "use server";
  back(await removeMember(fd));
}

export function PortalMembers({
  members,
  me,
  error,
  ok,
}: {
  members: Member[];
  me: { issuer: string; subject: string };
  error?: string;
  ok?: string;
}) {
  return (
    <div className="space-y-4">
      <h1 className="text-xl font-semibold">Members</h1>
      {error && <p role="alert" className="text-sm text-destructive">{error}</p>}
      {!error && ok && <p role="status" className="text-sm text-green-700">{ok}</p>}
      <form action={addAction} className="flex gap-2">
        <input name="email" type="email" required placeholder="member email" className="rounded border px-2 py-1" />
        <select name="role" defaultValue="member" className="rounded border px-2 py-1">
          <option value="member">member</option>
          <option value="manager">manager</option>
        </select>
        <button type="submit" className="rounded border px-3 py-1">Add</button>
      </form>
      <table className="w-full text-sm">
        <tbody>
          {members.map((m) => (
            <tr key={`${m.issuer}|${m.subject}`}>
              <td>{m.email ?? m.subject}{m.issuer === me.issuer && m.subject === me.subject ? " (you)" : ""}</td>
              <td>{m.role}</td>
              <td className="text-right">
                {m.role !== "owner" && (
                  <form action={removeAction}>
                    <input type="hidden" name="issuer" value={m.issuer} />
                    <input type="hidden" name="subject" value={m.subject} />
                    <button type="submit" className="underline">Remove</button>
                  </form>
                )}
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
