import { readFileSync } from "node:fs";
import { join } from "node:path";
import { getDb } from "@/lib/db";

let applied = false;

/** Apply the idempotent auth schema once per process. */
export async function applyAuthSchema(): Promise<void> {
  if (applied) return;
  const read = (f: string) => readFileSync(join(process.cwd(), "db", f), "utf8");
  const sql = `${read("auth-schema.sql")}\n${read("connect-schema.sql")}`;
  await getDb().query(sql);
  applied = true;
}
