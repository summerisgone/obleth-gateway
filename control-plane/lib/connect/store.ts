export interface Queryable {
  query(sql: string, params?: unknown[]): Promise<{ rows: any[] }>;
}

export interface CodeRow {
  codeHash: string;
  codeChallenge: string;
  redirectUri: string;
  userId: string;
  tenantId: string;
  keyName: string;
  expiryDays: number | null;
  app: string;
  device: string;
  createdAt: Date;
  expiresAt: Date;
  usedAt: Date | null;
}

export interface KeyRecord {
  keyId: string;
  keyHash: string;
  userId: string;
  tenantId: string;
  keyName: string;
  expiresAt: Date | null;
}

export interface CodeStore {
  /** Store a code; rows older than an hour are deleted in the same call. */
  insert(row: Omit<CodeRow, "usedAt">): Promise<void>;
  /** Atomically mark an unused, unexpired code used and return it; null if it cannot be redeemed. */
  redeem(codeHash: string, now: Date): Promise<CodeRow | null>;
}

export interface KeyStore {
  insert(rec: KeyRecord): Promise<void>;
  findByHash(keyHash: string): Promise<KeyRecord | null>;
  remove(keyId: string): Promise<void>;
  expired(now: Date): Promise<KeyRecord[]>;
}

const codeFromRow = (r: any): CodeRow => ({
  codeHash: r.code_hash,
  codeChallenge: r.code_challenge,
  redirectUri: r.redirect_uri,
  userId: r.user_id,
  tenantId: r.tenant_id,
  keyName: r.key_name,
  expiryDays: r.expiry_days,
  app: r.app,
  device: r.device,
  createdAt: r.created_at,
  expiresAt: r.expires_at,
  usedAt: r.used_at,
});

const keyFromRow = (r: any): KeyRecord => ({
  keyId: r.key_id,
  keyHash: r.key_hash,
  userId: r.user_id,
  tenantId: r.tenant_id,
  keyName: r.key_name,
  expiresAt: r.expires_at,
});

export function pgCodeStore(db: Queryable): CodeStore {
  return {
    async insert(c) {
      await db.query("delete from connect_codes where created_at < $1::timestamptz - interval '1 hour'", [c.createdAt]);
      await db.query(
        `insert into connect_codes
           (code_hash, code_challenge, redirect_uri, user_id, tenant_id, key_name, expiry_days, app, device, created_at, expires_at)
         values ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)`,
        [c.codeHash, c.codeChallenge, c.redirectUri, c.userId, c.tenantId, c.keyName, c.expiryDays, c.app, c.device, c.createdAt, c.expiresAt],
      );
    },
    async redeem(codeHash, now) {
      // One statement decides unused and unexpired and marks the row used, so
      // two exchanges of one code cannot both get a row.
      const { rows } = await db.query(
        `update connect_codes set used_at = $2
          where code_hash = $1 and used_at is null and expires_at > $2
          returning *`,
        [codeHash, now],
      );
      return rows[0] ? codeFromRow(rows[0]) : null;
    },
  };
}

export function pgKeyStore(db: Queryable): KeyStore {
  return {
    async insert(k) {
      await db.query(
        "insert into connect_keys (key_id, key_hash, user_id, tenant_id, key_name, expires_at) values ($1,$2,$3,$4,$5,$6)",
        [k.keyId, k.keyHash, k.userId, k.tenantId, k.keyName, k.expiresAt],
      );
    },
    async findByHash(keyHash) {
      const { rows } = await db.query("select * from connect_keys where key_hash = $1", [keyHash]);
      return rows[0] ? keyFromRow(rows[0]) : null;
    },
    async remove(keyId) {
      await db.query("delete from connect_keys where key_id = $1", [keyId]);
    },
    async expired(now) {
      const { rows } = await db.query("select * from connect_keys where expires_at is not null and expires_at <= $1 limit 200", [now]);
      return rows.map(keyFromRow);
    },
  };
}
