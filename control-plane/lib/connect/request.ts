import { isCodeChallenge } from "./pkce";
import { parseRedirectUri } from "./redirect-uri";

export interface ConnectParams {
  redirectUri: string;
  state: string;
  codeChallenge: string;
  app: string;
  device: string;
}

export type ConnectRequest =
  | { kind: "bad-redirect" }
  | { kind: "invalid"; redirectUri: string; state: string | null }
  | { kind: "ok"; params: ConnectParams };

const STATE = /^[\x21-\x7e]{1,512}$/;
const MAX_NAME = 64;

/** A display name: control characters out, trimmed, at most 64 characters. */
export function displayName(raw: unknown, fallback: string): string {
  if (typeof raw !== "string") return fallback;
  // eslint-disable-next-line no-control-regex
  const clean = raw.replace(/[\u0000-\u001f\u007f]+/g, " ").trim();
  return Array.from(clean).slice(0, MAX_NAME).join("").trim() || fallback;
}

type Query = Record<string, string | string[] | undefined>;
const one = (v: string | string[] | undefined) => (typeof v === "string" ? v : undefined);

/**
 * Validate a /connect query. An unusable redirect_uri is an error page, never a
 * redirect; any other bad parameter goes back to the (valid) redirect_uri.
 */
export function parseConnectRequest(q: Query): ConnectRequest {
  const redirectUri = parseRedirectUri(one(q.redirect_uri));
  if (!redirectUri) return { kind: "bad-redirect" };
  const state = one(q.state);
  const goodState = state !== undefined && STATE.test(state);
  if (
    !goodState ||
    !isCodeChallenge(one(q.code_challenge)) ||
    one(q.code_challenge_method) !== "S256"
  ) {
    return { kind: "invalid", redirectUri, state: goodState ? state : null };
  }
  return {
    kind: "ok",
    params: {
      redirectUri,
      state,
      codeChallenge: one(q.code_challenge)!,
      app: displayName(q.app, "Desktop app"),
      device: displayName(q.device, "device"),
    },
  };
}
