import { redirect } from "next/navigation";
import { getSession } from "@/lib/auth/session";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { OblethLogo } from "@/components/obleth-logo";
import { signCsrf } from "@/lib/connect/csrf";
import { connectDeps } from "@/lib/connect/deps";
import { callbackUrl } from "@/lib/connect/redirect-uri";
import { parseConnectRequest } from "@/lib/connect/request";
import { csrfFields } from "./fields";

export const dynamic = "force-dynamic";

function Shell({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="flex min-h-screen items-center justify-center bg-background px-4">
      <Card className="w-full max-w-md border-border bg-card/80">
        <CardHeader className="space-y-3 text-center">
          <div className="flex justify-center">
            <OblethLogo size={56} />
          </div>
          <CardTitle className="text-lg">{title}</CardTitle>
        </CardHeader>
        <CardContent>{children}</CardContent>
      </Card>
    </div>
  );
}

const EXPIRY = [
  { value: "0", label: "Never" },
  { value: "30", label: "30 days" },
  { value: "90", label: "90 days" },
];

export default async function ConnectPage({ searchParams }: { searchParams: Promise<Record<string, string | string[] | undefined>> }) {
  const query = await searchParams;
  const req = parseConnectRequest(query);
  if (req.kind === "bad-redirect") {
    return (
      <Shell title="Cannot connect">
        <p className="text-center text-sm text-muted-foreground">
          The application sent an invalid return address. Close this window and start the connection again from the application.
        </p>
      </Shell>
    );
  }
  if (req.kind === "invalid") redirect(callbackUrl(req.redirectUri, { error: "invalid_request" }, req.state));
  const { params } = req;

  const session = await getSession();
  if (!session) {
    const back = new URLSearchParams();
    for (const [k, v] of Object.entries(query)) if (typeof v === "string") back.set(k, v);
    redirect(`/login?next=${encodeURIComponent(`/connect?${back.toString()}`)}`);
  }
  if (session.status !== "active") redirect("/awaiting-approval");

  const deps = connectDeps();
  const identity = await deps.identity(session.id);
  const tenants = identity ? await deps.tenants(identity) : [];
  if (tenants.length === 0) {
    return (
      <Shell title="Cannot connect">
        <p className="text-center text-sm text-muted-foreground">
          {identity
            ? "Your account has no tenant yet. An administrator needs to assign you one."
            : "This account signs in without single sign-on and cannot issue keys this way. Create a key on the portal Keys page."}
        </p>
      </Shell>
    );
  }
  const preselected = tenants.find((t) => t.kind === "personal") ?? tenants[0]!;
  const csrf = signCsrf(session.id, csrfFields(params));

  return (
    <Shell title="Connect an application">
      <p className="mb-4 text-center text-sm text-muted-foreground">
        <strong className="text-foreground">{params.app}</strong> on <strong className="text-foreground">{params.device}</strong> asks for an
        API key for <strong className="text-foreground">{session.email}</strong>.
      </p>
      <form method="post" action="/connect/decision" className="space-y-4">
        <input type="hidden" name="csrf" value={csrf} />
        <input type="hidden" name="redirect_uri" value={params.redirectUri} />
        <input type="hidden" name="state" value={params.state} />
        <input type="hidden" name="code_challenge" value={params.codeChallenge} />
        <input type="hidden" name="app" value={params.app} />
        <input type="hidden" name="device" value={params.device} />
        <div className="space-y-1">
          <label htmlFor="tenant_id" className="text-sm font-medium">Tenant</label>
          <select id="tenant_id" name="tenant_id" defaultValue={preselected.id} className="h-9 w-full rounded-md border border-border bg-background px-2 text-sm">
            {tenants.map((t) => (
              <option key={t.id} value={t.id}>
                {t.name}
                {t.kind === "personal" ? " (personal)" : ""}
              </option>
            ))}
          </select>
        </div>
        <div className="space-y-1">
          <label htmlFor="expiry_days" className="text-sm font-medium">Key expires</label>
          <select id="expiry_days" name="expiry_days" defaultValue="0" className="h-9 w-full rounded-md border border-border bg-background px-2 text-sm">
            {EXPIRY.map((e) => (
              <option key={e.value} value={e.value}>{e.label}</option>
            ))}
          </select>
        </div>
        <CardDescription className="text-xs">
          The key is named <code>dsh-{params.device}</code>; an older key of that name in the tenant is replaced. You can revoke it on the portal Keys page.
        </CardDescription>
        <div className="flex gap-2">
          <Button type="submit" name="decision" value="deny" variant="outline" className="flex-1">Deny</Button>
          <Button type="submit" name="decision" value="approve" className="flex-1">Approve</Button>
        </div>
      </form>
    </Shell>
  );
}
