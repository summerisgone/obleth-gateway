// Content Security Policy for every dashboard document, built per request in
// proxy.ts. Scripts are allowed by nonce: Next.js reads the nonce from the
// request's Content-Security-Policy header and stamps it on every script tag
// it emits, and 'strict-dynamic' extends trust to the chunks those scripts
// load. Nothing else — no inline script without the nonce, no eval — runs in
// production. Development keeps 'unsafe-eval' for React Refresh and source
// maps only.
//
// Styles stay on 'unsafe-inline': Next's font loader, Recharts, and shadcn
// primitives write style attributes that a nonce cannot cover.
//
// Media may come from blob: URLs: the Playground's Speech mode assembles the
// audio it streamed in the browser and replays it from an object URL.
//
// fetch() may read blob: and data: URLs, which are the browser's own memory,
// never the network: the 3D viewer loads a downloaded model from an object
// URL (and three.js fetches the model's embedded textures the same way), and
// an image handed from Image mode to 3D mode is read back from its URL.

export interface CspOptions {
  development?: boolean;
}

export function contentSecurityPolicy(nonce: string, opts: CspOptions = {}): string {
  const development = opts.development ?? process.env.NODE_ENV === "development";
  return [
    "default-src 'self'",
    `script-src 'self' 'nonce-${nonce}' 'strict-dynamic'${development ? " 'unsafe-eval'" : ""}`,
    "style-src 'self' 'unsafe-inline'",
    "img-src 'self' data: blob:",
    "media-src 'self' blob:",
    "font-src 'self' data:",
    "connect-src 'self' blob: data:",
    "frame-ancestors 'none'",
    "base-uri 'self'",
    "form-action 'self'",
    "object-src 'none'",
  ].join("; ");
}

/** A fresh base64 nonce per document; 128 bits of randomness. */
export function generateNonce(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return btoa(String.fromCharCode(...bytes));
}
