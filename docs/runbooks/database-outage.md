# Runbook: Database Outage

**Incident Type:** PostgreSQL unavailable or severely degraded  
**Severity:** CRITICAL — Indexer stops ingesting; API returns 503; user data inaccessible

---

## Detection

### Automatic Signals
- **Health endpoint:** `GET /health` returns 503 with `"database": { "status": "down" }`
- **Prometheus:** `indexer_stalled` == 1 (writes fail, last progress timestamp goes stale)
- **Logs:** `P1001: Can't reach database server`, `Connection terminated unexpectedly`

### Manual Check
```bash
# Test API health
curl http://localhost:4000/health | jq '.checks.database'

# Test DB directly
docker compose ps db   # verify container status

psql "$DATABASE_URL" -c "SELECT 1"  # verify connectivity
```

---

## Containment

**DO:**
- ✅ The indexer will retry connections automatically; avoid repeated restarts
- ✅ Redis cache may still serve recent API responses to users
- ✅ Smart contracts operate independently — funds are safe; on-chain state is unaffected
- ✅ Notify users via status page if outage exceeds 5 minutes

**DO NOT:**
- ❌ Run database migrations (`prisma migrate deploy`) during an active outage
- ❌ Point `DATABASE_URL` at a read replica for writes — the indexer requires a primary
- ❌ Delete or truncate tables to "clear" the problem without understanding root cause
- ❌ Forcibly terminate the indexer process — let it retry; forced kill may leave a checkpoint in `applying` state

---

## A. Container / Process Failure

```bash
# Check container status
docker compose ps db

# Restart if stopped
docker compose up -d db

# Wait for health check (up to 30 seconds)
docker compose ps db

# Verify connection pool recovery
docker compose logs indexer | tail -20 | grep -iE 'database|prisma|connection'

# Expected: "Database connection re-established"
```

---

## B. Connection Pool Exhaustion

### Symptoms
- Logs: `Timed out fetching a connection from the pool`
- Metric: `http_request_duration_seconds{status="500"}` spikes
- `DB_HEALTH_WARN_THRESHOLD_MS` exceeded in logs

```bash
# Check pg_stat_activity for active connections
psql "$DATABASE_URL" -c "
  SELECT state, COUNT(*) as count
  FROM pg_stat_activity
  WHERE datname = current_database()
  GROUP BY state
  ORDER BY count DESC;
"

# Check for long-running queries blocking connections
psql "$DATABASE_URL" -c "
  SELECT pid, now() - pg_stat_activity.query_start AS duration,
         query, state
  FROM pg_stat_activity
  WHERE state != 'idle'
    AND (now() - pg_stat_activity.query_start) > interval '30 seconds'
  ORDER BY duration DESC;
"

# If blocking query found, terminate (ONLY if safe to do so)
# psql "$DATABASE_URL" -c "SELECT pg_terminate_backend(<pid>);"
```

**Tuning for pool exhaustion:**
```bash
# In indexer/.env, increase limits and restart:
DB_CONNECTION_LIMIT=15         # was 10
DB_WRITE_CONNECTION_LIMIT=5    # was 3
DB_POOL_TIMEOUT=45             # was 30

docker compose restart indexer
```

**Verification:** `pg_stat_activity` shows connection count drops below `POSTGRES_MAX_CONNECTIONS` (default 50).

---

## C. Disk Full

### Symptoms
- Logs: `FATAL: could not write to file`, `could not extend file`
- Docker or host shows disk at 100%

```bash
# Check Docker disk usage
docker system df

# Check host disk
df -h /var/lib/docker

# Check PostgreSQL data directory size
du -sh /var/lib/postgresql/data 2>/dev/null \
  || docker compose exec db du -sh /var/lib/postgresql/data
```

**Remediation:**
```bash
# 1. Free space first — remove old Docker images
docker image prune -f
docker volume prune -f  # ONLY unused volumes

# 2. Identify large tables
psql "$DATABASE_URL" -c "
  SELECT relname AS table,
         pg_size_pretty(pg_total_relation_size(relid)) AS total_size
  FROM pg_stat_user_tables
  ORDER BY pg_total_relation_size(relid) DESC
  LIMIT 10;
"

# 3. If MarketplaceEvent table is very large, archive old confirmed events
# (Only after consulting the data retention policy)
```

---

## D. Corrupt / Stuck Checkpoint (after partial write)

If the indexer crashed during a write, a `LedgerCheckpoint` may be stuck in `applying` state:

```bash
# Find stuck checkpoints
psql "$DATABASE_URL" -c "
  SELECT id, \"contractId\", \"windowStart\", \"windowEnd\", status, \"updatedAt\"
  FROM \"LedgerCheckpoint\"
  WHERE status IN ('applying', 'fetched')
  ORDER BY \"updatedAt\";
"
```

The indexer automatically replays these on restart (all event upserts are idempotent via `eventHash` unique constraint). If a checkpoint is > 30 minutes old in `applying` state with no progress:

```bash
# Reset stuck checkpoints (safe due to idempotent replays)
psql "$DATABASE_URL" -c "
  UPDATE \"LedgerCheckpoint\"
  SET status = 'fetched', error = 'reset_by_operator'
  WHERE status = 'applying'
    AND \"updatedAt\" < NOW() - INTERVAL '30 minutes';
"

# Restart indexer to trigger replay
docker compose restart indexer
```

---

## Determining Data Staleness

| Signal | Interpretation |
|---|---|
| `GET /readyz` returns 503 | DB down or sync stalled — data may be stale |
| `sync_lag.lagLedgers` > 100 | Data more than ~500 seconds behind |
| `SyncState.updatedAt` timestamp | Last time the DB was successfully written to |
| `MarketplaceEvent.ledgerSequence` max value | Last ledger recorded in the event log |

```bash
# When was the database last successfully written to?
psql "$DATABASE_URL" -c "
  SELECT \"lastLedger\", \"updatedAt\",
         EXTRACT(EPOCH FROM (NOW() - \"updatedAt\")) / 60 AS minutes_ago
  FROM \"SyncState\"
  WHERE id = 1;
"

# How many recent events are unconfirmed vs confirmed?
psql "$DATABASE_URL" -c "
  SELECT confirmed, COUNT(*) as events
  FROM \"MarketplaceEvent\"
  WHERE \"ledgerSequence\" > (
    SELECT MAX(\"lastLedger\") - 50 FROM \"SyncState\"
  )
  GROUP BY confirmed;
"
```

---

## User Communication

```
[Status Page — DB Outage]
ElcareHub is experiencing database connectivity issues.
New listings, bids, and marketplace activity may be delayed.
Your funds and assets on the Stellar blockchain are not affected.
We are working to restore full service. Next update in 15 minutes.
```

---

## Post-Recovery Checklist

```bash
# 1. Verify DB is healthy
curl http://localhost:4000/health | jq '.checks.database'
# Expected: {"status": "ok"}

# 2. Verify indexer resumes ingestion
watch -n 5 'curl -s http://localhost:4000/metrics | grep indexer_latest_ledger_processed'
# Ledger number should advance

# 3. Check for missed ledger gaps
psql "$DATABASE_URL" -c "SELECT * FROM \"LedgerGap\" WHERE status = 'Open';"

# 4. If gaps found, trigger repair
curl -X POST http://localhost:4000/admin/trigger-gap-repair \
  -H "Authorization: Bearer $HEALTH_DETAILS_TOKEN"

# 5. Verify dead-letter events (parse failures during recovery)
psql "$DATABASE_URL" -c "
  SELECT COUNT(*) FROM \"DeadLetterEvent\" WHERE status = 'Pending';
"
# If > 0, replay after root cause is fixed:
# cd indexer && npm run dead-letter-replay
```

---

## Restore Drill: Exercising a Backup in a Disposable Environment

A backup is not verified until a restore has been exercised in a disposable
environment. The `db-backup` workflow performs that restore automatically after
every backup (`verify-restore` job), but the drill itself must be scheduled,
evidenced, and repeatable. This section is the drill runbook; it uses the existing
`db-backup` workflow and this runbook only — no new tooling, no production writes.

### Prerequisites

| # | Prerequisite | How to confirm |
|---|---|---|
| 1 | `db-backup` workflow exists with its `verify-restore` job (`.github/workflows/db-backup.yml`) | File present; latest run shows the `Restore Verification (DR Drill)` job |
| 2 | Required Actions secrets configured: `BACKUP_DATABASE_URL`, `BACKUP_ENCRYPTION_KEY` (optional: `METRICS_PUSH_URL`, `ALERT_WEBHOOK_URL`) | The `PostgreSQL Backup` job passes its "Run backup" step |
| 3 | Disposable restore target: the CI `postgres:15-alpine` service container, a local Docker database, or a staging database — **never production** | `RESTORE_TARGET_URL` points at a `localhost` / staging host, not the production host |
| 4 | Helper scripts present and syntax-clean | `bash -n scripts/backup/backup.sh scripts/backup/restore.sh` |
| 5 | Operator can open Actions run logs and download the backup artifact | Run URL loads; artifact `db-backup-<run_id>` listed |

**Credentials stay outside the repository.** The production connection string and
the encryption passphrase exist only as GitHub Actions secrets (and the operator's
password manager). They are never committed, never echoed by `backup.sh` /
`restore.sh`, and never recorded in this runbook, an issue, or a drill report — use
placeholders such as `<RUN_URL>` and `<BACKUP_ARTIFACT_NAME>`, as the
[Backup Retention Verification Report](../reliability/backup-retention-verification-report.md)
requires. The disposable target uses throwaway drill credentials only.

### Procedure

1. **Trigger** — Actions → *Database Backup* → *Run workflow* (`workflow_dispatch`), or wait for the nightly `0 2 * * *` schedule.
2. **Backup** — the `backup` job produces `pg_backup_<timestamp>.dump.enc` plus its `.sha256` and uploads artifact `db-backup-<run_id>` (90-day retention, `if-no-files-found: error`).
3. **Restore (disposable)** — the `verify-restore` job starts a throwaway `postgres:15-alpine` container (`restore_test` user/database on the job-local network), downloads the artifact, and runs `scripts/backup/restore.sh`: checksum verification → encryption header check → `pg_restore --clean --if-exists` → `prisma migrate deploy` → read checks (`Listing`, `Auction`, `Offer`, `MarketplaceEvent`, `SyncState`).
4. **Evidence** — the job summary renders the *DB Restore Verification* table and `restore.sh` prints `[report] RESULT: PASS`. Only aggregate numeric metrics go to the optional Pushgateway.
5. **Operator drill (optional local equivalent)** — run the same script against a disposable target:

   ```bash
   export BACKUP_FILE=/path/to/pg_backup_<timestamp>.dump.enc   # downloaded CI artifact
   export BACKUP_ENCRYPTION_KEY='<from the password manager — never commit>'
   export RESTORE_TARGET_URL='postgresql://<drill_user>:<drill_pass>@localhost:5432/drill_db'
   bash scripts/backup/restore.sh
   ```

### Success criteria

The drill passes only when **all** of the following hold. Each row is measurable
from the workflow run or the local script output:

| # | Criterion | Measured by |
|---|---|---|
| 1 | Encrypted artifact + checksum produced | `db-backup-<run_id>` artifact exists (`if-no-files-found: error`) |
| 2 | Checksum verified | `restore.sh` checksum step reports `ok`; script exit code `0` |
| 3 | Encryption passphrase correct | header decrypt check passes |
| 4 | Restore completed | `pg_restore complete (<n>s)` in the log |
| 5 | Schema current on target | `prisma migrate deploy` status `ok` |
| 6 | Data readable | read checks return row counts and `lastLedger` with no `ERROR` |
| 7 | Backup is fresh | artifact age < 25 hours (the `restore.sh` warning threshold; ≥ 25 h means a scheduled run was missed → escalate) |
| 8 | Overall verdict | `Restore Verification (DR Drill)` job = success **and** `[report] RESULT: PASS` |

Any failed row = drill failure: treat the newest backup as **unverified** and
escalate per the table below. Artifact presence alone never counts as verification.

### Ownership, trigger, expected output, rollback / escalation

| | |
|---|---|
| **Owner** | DevOps Engineer (primary — see [Owner & Contacts](#owner--contacts)); Backend Lead (secondary) |
| **Trigger** | Nightly `0 2 * * *` schedule; manual `workflow_dispatch`; quarterly drill evidence recorded in the [Backup Retention Verification Report](../reliability/backup-retention-verification-report.md) |
| **Expected output** | Green `verify-restore` job, `[report] RESULT: PASS`, job-summary table, artifact retained 90 days, drill report filed with placeholders only |
| **Rollback / remediation** | The drill only ever writes to a disposable database: tear the target down (`docker compose down -v` for a local target) and discard the throwaway credentials. No production state is modified, so there is nothing to roll back in production. If the artifact itself is suspect, re-run the `db-backup` workflow to produce a fresh one |
| **Escalation** | Failed or red `verify-restore` → DevOps (Slack `#alerts`, `ops@elcarehub.xyz`); still failing after one re-run → Backend Lead (phone, urgent); a missed 02:00 run or artifact age ≥ 25 h → inspect workflow run history and re-run `db-backup` before relying on that backup for DR |

---

## Prohibited Actions

- ❌ Do not run `DROP TABLE`, `TRUNCATE`, or delete indexes during recovery
- ❌ Do not change `DATABASE_URL` to a different database without migrating schema
- ❌ Do not apply Prisma migrations while an outage is active
- ❌ Do not restore from backup without stopping the indexer first (applies to the live/staging database — a disposable drill target is unaffected, see [Restore Drill](#restore-drill-exercising-a-backup-in-a-disposable-environment))

---

## Post-Incident Review Template

```markdown
## Incident: Database Outage — [Date]

**Duration:** [start] to [resolution]
**Root Cause:** [container crash | pool exhaustion | disk full | hardware]
**Impact:** API returned 503 for [N] minutes; indexer fell [X] ledgers behind

**Timeline:**
- T+0: /health reported database down
- T+Xm: Root cause identified ([cause])
- T+Ym: Remediation applied
- T+Zm: DB recovered; indexer resumed

**Action Items:**
- [ ] Set up database backup/restore runbook
- [ ] Enable managed PostgreSQL (Railway / RDS / Supabase) for HA
- [ ] Add disk usage alert at 80% capacity
- [ ] Review connection pool limits
```

---

## Owner & Contacts

| Role | Contact | Escalation |
|---|---|---|
| **Primary:** DevOps | ops@elcarehub.xyz | Slack #alerts |
| **Secondary:** Backend Lead | backend-lead@elcarehub.xyz | Phone (urgent) |

---

## Related Runbooks

- [Stalled Ingestion](./stalled-ingestion.md)
- [Redis Outage](./redis-outage.md)

---

## Database Connection Pool Sizing Matrix & Failure Validation

### Sizing Principles & Pool Separation
The indexer divides PostgreSQL connections into two dedicated pools:
1. **Read Pool (`indexer/src/db.ts`):** Serves API read traffic (`/listings`, `/auctions`, `/stats`). Controlled by `DB_CONNECTION_LIMIT`.
2. **Write Pool (`indexer/src/prisma-write.ts`):** Dedicated to poller event ingestion, batch parser transactions, and backfill. Controlled by `DB_WRITE_CONNECTION_LIMIT`.

Dedicated separation ensures high API traffic never blocks on-chain event ingestion, and heavy event backfills never starve API clients.

### Environment Sizing Matrix

| Environment | Topology / Instances | Read Pool (`DB_CONNECTION_LIMIT`) | Write Pool (`DB_WRITE_CONNECTION_LIMIT`) | Total / Instance | Total Indexer Conns | PostgreSQL `max_connections` | Reserved Headroom |
|---|---|---|---|---|---|---|---|
| **Local Dev** (Docker) | 1 instance | 10 | 3 | 13 | 13 | 50 | 37 (74%) |
| **Staging** (Single Host) | 2 replicas | 15 | 5 | 20 | 40 | 100 | 60 (60%) |
| **Production (Standard)** | 4 replicas | 25 | 5 | 30 | 120 | 200 | 80 (40%) |
| **Production (High-Load)** | 8 replicas (with PgBouncer) | 30 (to pooler) | 5 (direct) | 35 | pooled | 300 | 100 (33%) |

**Sizing Rule of Thumb:**
$$\text{Total Connections} = \sum (\text{Replicas} \times (\text{ReadLimit} + \text{WriteLimit})) + \text{Headroom (15--20\%)}$$
Always verify that total connection allocation does not exceed `80%` of PostgreSQL's `max_connections`.

---

### Pool Acquisition Failure Diagnostics & Validation [READ-ONLY]

#### Diagnostic Step 1: Inspect Active vs Idle Connections [READ-ONLY]
```sql
-- [READ-ONLY] Check connection state distribution in PostgreSQL
SELECT state, count(*), max(now() - state_change) AS max_duration
FROM pg_stat_activity
WHERE datname = current_database()
GROUP BY state
ORDER BY count(*) DESC;
```

#### Diagnostic Step 2: Identify Blocked or Long-Running Queries [READ-ONLY]
```sql
-- [READ-ONLY] Locate transactions holding connections > 10 seconds
SELECT pid, usename, client_addr, state, now() - query_start AS duration, query
FROM pg_stat_activity
WHERE state != 'idle'
  AND (now() - query_start) > INTERVAL '10 seconds'
ORDER BY duration DESC;
```

#### Diagnostic Step 3: Validate Pool Health Metric [READ-ONLY]
```bash
# Check indexer DB health probe latency
curl -s http://localhost:4000/metrics | grep -E 'indexer_db_health|db_pool'
```

#### Common Error Codes & Remediation

| Symptom / Log Output | Root Cause | Operator Action |
|---|---|---|
| `Timed out fetching a new connection from the connection pool` (`P2024` / `P1001`) | Read pool exhausted under spike load | Increase `DB_CONNECTION_LIMIT` by 5; check for unindexed slow queries |
| `P1008: Operations timed out after 30000ms` | Slow statement holding connection | Inspect `pg_stat_activity`; reduce query complexity or increase `DB_STATEMENT_TIMEOUT` |
| `FATAL: remaining connection slots are reserved for non-replication superuser connections` | Total connections exceeded `max_connections` | Reduce per-instance pool limit or scale down unused replicas |

#### Revert Procedure
If pool adjustments cause database memory exhaustion or instability:
1. Revert `DB_CONNECTION_LIMIT` and `DB_WRITE_CONNECTION_LIMIT` in `.env` to default values (`10` and `3`).
2. Restart indexer containers: `docker compose restart indexer`.
3. Terminate stuck connections if necessary:
   ```sql
   SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = current_database() AND state = 'idle in transaction';
   ```

