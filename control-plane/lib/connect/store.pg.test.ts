// Runs against a real Postgres when CONNECT_TEST_DATABASE_URL is set (the
// database name must contain "test"); skipped otherwise. This is where "one of
// two concurrent exchanges wins" is proven, because the guarantee is the
// database's single UPDATE ... RETURNING.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { Pool } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { pgCodeStore, pgKeyStore } from "./store";

const url = process.env.CONNECT_TEST_DATABASE_URL;
const run = url && /test/.test(url.split("/").pop() ?? "") ? describe : describe.skip;

run("postgres stores", () => {
  let pool: Pool;
  const now = new Date("2026-11-19T10:00:00Z");
  const code = (hash: string, createdAt = now) => ({
    codeHash: hash, codeChallenge: "c".repeat(43), redirectUri: "http://localhost:1/obleth/callback", userId: "u1",
    tenantId: "00000000-0000-0000-0000-000000000001", keyName: "dsh-x", expiryDays: null, app: "a", device: "d",
    createdAt, expiresAt: new Date(createdAt.getTime() + 60_000),
  });

  beforeAll(async () => {
    pool = new Pool({ connectionString: url, max: 10 });
    await pool.query(`create table if not exists tenants (id uuid primary key, name text)`);
    await pool.query(`create table if not exists "user" (id text primary key)`);
    await pool.query(readFileSync(join(process.cwd(), "db", "connect-schema.sql"), "utf8"));
    await pool.query(`insert into tenants (id, name) values ('00000000-0000-0000-0000-000000000001','t') on conflict do nothing`);
    await pool.query(`insert into "user" (id) values ('u1') on conflict do nothing`);
    await pool.query("delete from connect_codes");
  });
  afterAll(async () => {
    await pool?.end();
  });

  it("applies its schema twice", async () => {
    await pool.query(readFileSync(join(process.cwd(), "db", "connect-schema.sql"), "utf8"));
  });

  it("lets exactly one of 20 concurrent redemptions win", async () => {
    const store = pgCodeStore(pool);
    await store.insert(code("race"));
    const results = await Promise.all(Array.from({ length: 20 }, () => store.redeem("race", new Date(now.getTime() + 1000))));
    expect(results.filter(Boolean)).toHaveLength(1);
  });
  it("refuses a code at 61 s and accepts it at 59 s", async () => {
    const store = pgCodeStore(pool);
    await store.insert(code("late"));
    await store.insert(code("ontime"));
    expect(await store.redeem("late", new Date(now.getTime() + 61_000))).toBeNull();
    expect(await store.redeem("ontime", new Date(now.getTime() + 59_000))).not.toBeNull();
  });
  it("deletes rows older than an hour when it writes", async () => {
    const store = pgCodeStore(pool);
    await store.insert(code("ancient", new Date(now.getTime() - 3_700_000)));
    await store.insert(code("fresh"));
    const { rows } = await pool.query("select code_hash from connect_codes where code_hash in ('ancient','fresh')");
    expect(rows.map((r) => r.code_hash)).toEqual(["fresh"]);
  });
  it("finds, expires and removes keys", async () => {
    const keys = pgKeyStore(pool);
    const id = "11111111-1111-1111-1111-111111111111";
    await keys.insert({ keyId: id, keyHash: "h1", userId: "u1", tenantId: "00000000-0000-0000-0000-000000000001", keyName: "dsh-x", expiresAt: new Date(now.getTime() + 1000) });
    expect((await keys.findByHash("h1"))?.keyId).toBe(id);
    expect(await keys.expired(now)).toHaveLength(0);
    expect((await keys.expired(new Date(now.getTime() + 2000))).map((k) => k.keyId)).toEqual([id]);
    await keys.remove(id);
    expect(await keys.findByHash("h1")).toBeNull();
  });
});
