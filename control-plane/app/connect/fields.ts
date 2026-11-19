import type { ConnectParams } from "@/lib/connect/request";

/** What the CSRF token of the consent form is bound to. */
export const csrfFields = (p: ConnectParams) => [p.redirectUri, p.state, p.codeChallenge, p.app, p.device];
