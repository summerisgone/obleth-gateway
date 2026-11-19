/** Next.js runs register() once at server startup (Node runtime only). */
export async function register() {
  if (process.env.NEXT_RUNTIME !== "nodejs") return;
  const { applyAuthSchema } = await import("@/lib/auth/schema");
  const { bootstrapAdmin } = await import("@/lib/auth/bootstrap");
  await applyAuthSchema();
  await bootstrapAdmin();
  // Keys issued by /connect with an expiry: the gateway has no key expiry, so
  // delete them here once it passes.
  const { startConnectSweeper } = await import("@/lib/connect/sweeper");
  startConnectSweeper();
  // Best effort and not awaited: the gateway may still be starting; a failure
  // is retried on the next start.
  const { backfillMemberships } = await import("@/lib/auth/backfill-memberships");
  backfillMemberships()
    .then((n) => { if (n) console.log(`backfillMemberships: moved ${n} user(s) to tenant members`); })
    .catch((err) => console.error("backfillMemberships failed", err));
}
