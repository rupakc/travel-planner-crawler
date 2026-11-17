//! `vcrawl migrate`: expand/contract, the contract gate and the N-1 rule (spec §8, §17.6).
//!
//! `migration_n_minus_1` replays the **previous image's queries** — a recorded set in
//! `testkit/fixtures/vcrawl/ops/n_minus_1_queries.json` — against the schema this tree builds:
//! every statement must still prepare (tables, columns, functions and types it names exist
//! with compatible types), and every recorded `INSERT` must still supply each `NOT NULL`
//! column that has no default. A migration that drops, renames or retypes something the
//! previous release uses, or adds a mandatory column, fails here.
//!
//! Re-record after a release (the set is the queries the *deployed* image issues):
//!
//! ```text
//! docker run -d --name pglog -e POSTGRES_PASSWORD=postgres -p 55439:5432 \
//!   pgvector/pgvector:pg17 postgres -c log_statement=all -c max_connections=500
//! VC_TEST_DATABASE_URL=postgres://postgres:postgres@localhost:55439/postgres \
//!   cargo test -p vc-api -p vc-frontier -p vc-index -p vc-llm -p vc-obs -p vc-queue \
//!              -p vc-sources -p vc-store -p vcrawl
//! docker logs pglog > /tmp/pg.log 2>&1
//! VC_RECORD_PG_LOG=/tmp/pg.log VC_RECORD_FROM=$(git rev-parse --short HEAD) \
//!   cargo test -p vcrawl --test migrate -- --ignored record_n_minus_1_query_set
//! ```

mod admin_common;

use std::collections::{BTreeMap, BTreeSet};

use admin_common::*;
use chrono::{Duration, Utc};
use sqlx::{Executor, PgPool};
use vcrawl::migrate::{
    CONTRACT_TABLE, applied_versions, contract_gate, migrate_contract, migrate_expand,
    migration_versions, missing_migrations, read_rollbacks, record_image, schema_version,
};

const FIXTURE: &str = "n_minus_1_queries.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct QuerySet {
    recorded_from: String,
    note: String,
    queries: Vec<String>,
}

fn load_set() -> QuerySet {
    let text = std::fs::read_to_string(fixtures().join(FIXTURE)).expect("query set");
    serde_json::from_str(&text).expect("query set json")
}

/// Statements that fail to prepare on `pool`, with the error.
async fn prepare_failures(pool: &PgPool, queries: &[String]) -> Vec<(String, String)> {
    let mut conn = pool.acquire().await.expect("conn");
    let mut bad = Vec::new();
    for (i, q) in queries.iter().enumerate() {
        let r = conn
            .execute(sqlx::AssertSqlSafe(format!("PREPARE n1_{i} AS {q}")))
            .await;
        match r {
            Ok(_) => {
                conn.execute(sqlx::AssertSqlSafe(format!("DEALLOCATE n1_{i}")))
                    .await
                    .expect("deallocate");
            }
            Err(e) => bad.push((q.clone(), e.to_string())),
        }
    }
    bad
}

/// `INSERT INTO <table> (<cols>)` → (table, cols).
fn insert_target(q: &str) -> Option<(String, Vec<String>)> {
    let rest = q.trim_start().strip_prefix("INSERT INTO ")?;
    let name_end = rest.find(|c: char| c.is_whitespace() || c == '(')?;
    let table = rest[..name_end].trim_matches('"').to_string();
    let rest = rest[name_end..].trim_start();
    let inner = rest.strip_prefix('(')?;
    let close = inner.find(')')?;
    let cols = inner[..close]
        .split(',')
        .map(|c| c.trim().trim_matches('"').to_string())
        .collect();
    Some((table, cols))
}

#[tokio::test]
async fn migration_n_minus_1() {
    let set = load_set();
    assert!(
        set.queries.len() >= 100,
        "the recorded set is suspiciously small: {}",
        set.queries.len()
    );
    // The new schema: every expand migration of this tree.
    let pool = testkit::pg().await;
    let bad = prepare_failures(&pool, &set.queries).await;
    assert!(
        bad.is_empty(),
        "{} recorded queries of the previous image no longer prepare (expand-only, §17.6):\n{}",
        bad.len(),
        bad.iter()
            .map(|(q, e)| format!("  {e}\n    {q}"))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let mut mandatory: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT table_name::text, column_name::text FROM information_schema.columns
         WHERE table_schema = 'public' AND is_nullable = 'NO' AND column_default IS NULL
           AND is_identity = 'NO' AND is_generated = 'NEVER'",
    )
    .fetch_all(&pool)
    .await
    .expect("columns");
    for (t, c) in rows {
        mandatory.entry(t).or_default().insert(c);
    }
    let mut missing = Vec::new();
    let mut inserts = 0;
    for q in &set.queries {
        let Some((table, cols)) = insert_target(q) else {
            continue;
        };
        inserts += 1;
        let cols: BTreeSet<String> = cols.into_iter().collect();
        for need in mandatory.get(&table).into_iter().flatten() {
            if !cols.contains(need) {
                missing.push(format!("{table}.{need} in: {q}"));
            }
        }
    }
    assert!(inserts >= 10, "the set has {inserts} inserts");
    assert!(
        missing.is_empty(),
        "new NOT NULL columns without a default break the previous image's inserts:\n{}",
        missing.join("\n")
    );
}

#[tokio::test]
async fn n_minus_1_check_catches_a_breaking_migration() {
    let pool = testkit::pg().await;
    let set = load_set();
    let uses = |needle: &str| set.queries.iter().any(|q| q.contains(needle));
    assert!(uses("park_reason") && uses("INSERT INTO records "));
    // What a careless migration would do: rename a column, add a mandatory one.
    pool.execute(
        "ALTER TABLE politeness_keys RENAME COLUMN park_reason TO parked_why;
         ALTER TABLE records ADD COLUMN shard integer NOT NULL",
    )
    .await
    .expect("break the schema");
    let bad = prepare_failures(&pool, &set.queries).await;
    assert!(
        bad.iter().any(|(_, e)| e.contains("park_reason")),
        "a renamed column must be caught"
    );
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_name = 'records' AND is_nullable = 'NO' AND column_default IS NULL",
    )
    .fetch_all(&pool)
    .await
    .expect("cols");
    assert!(cols.contains(&"shard".to_string()));
    let covered = set
        .queries
        .iter()
        .filter_map(|q| insert_target(q))
        .filter(|(t, _)| t == "records")
        .all(|(_, c)| c.contains(&"shard".to_string()));
    assert!(
        !covered,
        "the recorded records insert lacks the new mandatory column"
    );
}

/// Statements that only expand: no drops, renames, retypes or new mandatory columns; each
/// migration sets `lock_timeout` first (§17.6). Contract steps live in `migrations/contract/`.
#[test]
fn expand_only_lint() {
    let dir = repo().join("migrations");
    let mut problems = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("migrations")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    files.sort();
    assert!(files.len() >= 12);
    for f in files {
        let name = f.file_name().expect("name").to_string_lossy().to_string();
        let text = std::fs::read_to_string(&f).expect("read");
        let code: String = text
            .lines()
            .map(|l| l.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        let first = code
            .split(';')
            .map(str::trim)
            .find(|s| !s.is_empty())
            .unwrap_or("");
        if !first.to_ascii_uppercase().starts_with("SET LOCK_TIMEOUT") {
            problems.push(format!("{name}: first statement must be SET lock_timeout"));
        }
        problems.extend(
            expand_violations(&code)
                .into_iter()
                .map(|v| format!("{name}: {v}")),
        );
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

fn expand_violations(sql: &str) -> Vec<String> {
    let up = sql
        .to_ascii_uppercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut v = Vec::new();
    for bad in [
        "DROP TABLE",
        "DROP COLUMN",
        "DROP VIEW",
        "DROP FUNCTION",
        "DROP TYPE",
        "RENAME ",
        "SET NOT NULL",
        "SET DATA TYPE",
        "TRUNCATE ",
    ] {
        if up.contains(bad) {
            v.push(format!("contains {bad:?}"));
        }
    }
    let mut rest = up.as_str();
    while let Some(i) = rest.find("ALTER COLUMN ") {
        let clause = &rest[i..rest[i..].find([',', ';']).map_or(rest.len(), |j| i + j)];
        if clause.contains(" TYPE ") {
            v.push(format!("retypes a column: {clause}"));
        }
        rest = &rest[i + 13..];
    }
    let mut rest = up.as_str();
    while let Some(i) = rest.find("ADD COLUMN ") {
        let clause = &rest[i..rest[i..].find([',', ';']).map_or(rest.len(), |j| i + j)];
        if clause.contains("NOT NULL") && !clause.contains("DEFAULT") {
            v.push(format!("mandatory column without a default: {clause}"));
        }
        rest = &rest[i + 11..];
    }
    v
}

#[test]
fn expand_lint_flags_breaking_statements() {
    assert!(expand_violations("ALTER TABLE t ADD COLUMN a int NOT NULL DEFAULT 0;").is_empty());
    assert!(expand_violations("ALTER TABLE t ADD COLUMN a int;").is_empty());
    assert_eq!(
        expand_violations("ALTER TABLE t ADD COLUMN a int NOT NULL, ADD COLUMN b int;").len(),
        1
    );
    assert_eq!(expand_violations("alter table t drop column a;").len(), 1);
    assert_eq!(
        expand_violations("ALTER TABLE t ALTER COLUMN a TYPE bigint;").len(),
        1
    );
    assert_eq!(expand_violations("ALTER TABLE t RENAME TO u;").len(), 1);
}

#[tokio::test]
async fn migrate_expand_is_idempotent_and_tolerates_newer_schema() {
    let pool = testkit::pg().await;
    assert_eq!(migrate_expand(&pool, "img-a").await.expect("expand"), 0);
    assert!(schema_version() >= 1501);
    // A newer image already applied a migration this one does not ship (rollback case).
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (999901, 'from the future', true, '\\x00', 1)",
    )
    .execute(&pool)
    .await
    .expect("future");
    assert_eq!(migrate_expand(&pool, "img-a").await.expect("again"), 0);
    let first: serde_json::Value = sqlx::query_scalar(
        "SELECT progress FROM heartbeats WHERE role = 'image' AND instance = 'img-a'",
    )
    .fetch_one(&pool)
    .await
    .expect("image row");
    assert!(first["first_seen"].is_string());
}

#[tokio::test]
async fn migrate_contract_waits_for_seven_quiet_days() {
    let pool = testkit::pg().await;
    let now = Utc::now();
    let e = migrate_contract(&pool, "img-b", now, &[], 7)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("never ran"), "{e}");
    record_image(&pool, "img-b").await.expect("seen");
    let e = migrate_contract(&pool, "img-b", now, &[], 7)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("days are required"), "{e}");

    // Eight days later, after a rollback three days ago: refused.
    sqlx::query(
        "UPDATE heartbeats SET progress = jsonb_build_object('first_seen', now() - interval '8 days'),
                               last_beat = now() - interval '8 days'
         WHERE role = 'image' AND instance = 'img-b'",
    )
    .execute(&pool)
    .await
    .expect("age");
    let e = migrate_contract(&pool, "img-b", now, &[now - Duration::days(3)], 7)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("rollback"), "{e}");
    let applied = migrate_contract(&pool, "img-b", now, &[now - Duration::days(9)], 7)
        .await
        .expect("contract");
    assert_eq!(applied, 0, "this release ships no contract migration");
    assert_eq!(
        count(
            &pool,
            &format!("SELECT count(*) FROM pg_tables WHERE tablename = '{CONTRACT_TABLE}'")
        )
        .await,
        1
    );
    // Another image migrating inside the window blocks it again.
    record_image(&pool, "img-c").await.expect("other");
    let e = migrate_contract(&pool, "img-b", now, &[], 7)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("img-c"), "{e}");
}

#[test]
fn contract_gate_rules() {
    let now = Utc::now();
    let d = Duration::days;
    assert!(contract_gate(now, None, &[], &[], 7).is_err());
    assert!(contract_gate(now, Some(now - d(6)), &[], &[], 7).is_err());
    assert!(contract_gate(now, Some(now - d(7)), &[], &[], 7).is_ok());
    let old = vec![("prev".to_string(), now - d(8))];
    assert!(contract_gate(now, Some(now - d(10)), &old, &[now - d(30)], 7).is_ok());
    let recent = vec![("prev".to_string(), now - d(2))];
    assert!(contract_gate(now, Some(now - d(10)), &recent, &[], 7).is_err());
    assert!(contract_gate(now, Some(now - d(10)), &[], &[now - d(1)], 7).is_err());
}

#[test]
fn rollback_log_parses() {
    let dir = tempfile::tempdir().expect("tmp");
    let p = dir.path().join("rollbacks");
    assert!(read_rollbacks(&p).expect("missing").is_empty());
    std::fs::write(&p, "2026-11-10T03:04:05Z\n\n2026-11-12T00:00:00Z\n").expect("write");
    assert_eq!(read_rollbacks(&p).expect("read").len(), 2);
    std::fs::write(&p, "yesterday\n").expect("write");
    assert!(read_rollbacks(&p).is_err());
}

// ---------------------------------------------------------------- recorder

/// Extracts the DML statements from a Postgres `log_statement=all` log.
fn statements_from_log(log: &str) -> Vec<String> {
    let is_entry = |l: &str| {
        l.len() > 24
            && l.as_bytes()[4] == b'-'
            && l.as_bytes()[10] == b' '
            && l.as_bytes()[..4].iter().all(u8::is_ascii_digit)
    };
    let mut entries: Vec<String> = Vec::new();
    for line in log.lines() {
        if is_entry(line) {
            entries.push(line.to_string());
        } else if let Some(last) = entries.last_mut() {
            last.push('\n');
            last.push_str(line);
        }
    }
    let mut out = BTreeSet::new();
    for e in entries {
        let Some(i) = e.find("LOG:  ") else { continue };
        let msg = &e[i + 6..];
        let sql = if let Some(s) = msg.strip_prefix("statement: ") {
            s
        } else if let Some(s) = msg.strip_prefix("execute ") {
            match s.split_once(": ") {
                Some((_, sql)) => sql,
                None => continue,
            }
        } else {
            continue;
        };
        let sql: String = sql
            .lines()
            .map(|l| l.split("--").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let sql = sql.trim_end_matches(';').trim().to_string();
        let head = sql
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        let dml = matches!(
            head.as_str(),
            "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "WITH"
        );
        let noise = sql.contains("_sqlx_migrations")
            || sql.contains("pg_advisory")
            || sql.contains("pg_catalog")
            || sql.contains("information_schema")
            || sql.contains("pg_database")
            || sql.contains(';');
        if dml && !noise {
            out.insert(sql);
        }
    }
    out.into_iter().collect()
}

#[test]
fn recorder_parses_postgres_logs() {
    let log = "2026-11-16 10:00:00.123 UTC [42] LOG:  execute sqlx_s_1: SELECT a\n\
               \t  FROM t -- note\n\
               \t WHERE b = $1\n\
               2026-11-16 10:00:00.124 UTC [42] DETAIL:  Parameters: $1 = 'x'\n\
               2026-11-16 10:00:00.125 UTC [42] LOG:  statement: BEGIN\n\
               2026-11-16 10:00:00.126 UTC [42] LOG:  execute <unnamed>: INSERT INTO t (a) VALUES ($1)\n\
               2026-11-16 10:00:00.127 UTC [42] LOG:  execute sqlx_s_2: SELECT a FROM t WHERE b = $1\n";
    assert_eq!(
        statements_from_log(log),
        vec![
            "INSERT INTO t (a) VALUES ($1)".to_string(),
            "SELECT a FROM t WHERE b = $1".to_string()
        ]
    );
    assert_eq!(
        insert_target("INSERT INTO t (a, \"b\") VALUES ($1, $2)"),
        Some(("t".to_string(), vec!["a".to_string(), "b".to_string()]))
    );
}

/// Writes the fixture from a recorded Postgres log (see the module docs). Keeps only the
/// statements that prepare on the current schema.
#[tokio::test]
#[ignore = "re-records the N-1 query set; run by hand after a release"]
async fn record_n_minus_1_query_set() {
    let path = std::env::var("VC_RECORD_PG_LOG").expect("VC_RECORD_PG_LOG");
    let log = std::fs::read_to_string(path).expect("log");
    let all = statements_from_log(&log);
    let pool = testkit::pg().await;
    let bad: BTreeSet<String> = prepare_failures(&pool, &all)
        .await
        .into_iter()
        .map(|(q, _)| q)
        .collect();
    let queries: Vec<String> = all.into_iter().filter(|q| !bad.contains(q)).collect();
    let set = QuerySet {
        recorded_from: std::env::var("VC_RECORD_FROM").unwrap_or_else(|_| "unknown".into()),
        note: "DML the crates issue against Postgres, recorded from their test suites with \
               log_statement=all; replayed by crates/vcrawl/tests/migrate.rs"
            .into(),
        queries,
    };
    let text = serde_json::to_string_pretty(&set).expect("json") + "\n";
    std::fs::write(fixtures().join(FIXTURE), text).expect("write fixture");
}

/// `vcrawl migrate --print-schema` (read at image build time for the
/// `io.voyager-crawler.{schema,min-schema}` labels): the highest expand migration in
/// `migrations/`, without config or a database.
#[test]
fn print_schema_reports_the_highest_expand_migration() {
    let highest = std::fs::read_dir(repo().join("migrations"))
        .expect("migrations")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let v = name.strip_suffix(".sql")?.split('_').next()?.to_string();
            v.parse::<i64>().ok()
        })
        .max()
        .expect("at least one migration");
    assert_eq!(schema_version(), highest);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_vcrawl"))
        .args(["migrate", "--print-schema"])
        // No database reachable: the command must not need one.
        .env("VC_DATABASE_URL", "postgres://nobody@127.0.0.1:1/none")
        .output()
        .expect("run vcrawl");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(v["schema"], highest);
    assert_eq!(v["min_schema"], highest);
    // The full set (owner blocks merge out of order: the highest number is not enough).
    let mut all: Vec<i64> = std::fs::read_dir(repo().join("migrations"))
        .expect("migrations")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".sql")?
                .split('_')
                .next()?
                .parse::<i64>()
                .ok()
        })
        .collect();
    all.sort_unstable();
    assert_eq!(migration_versions(), all);
    let list = all.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    assert_eq!(v["migrations"], list.as_str());
    let bad = std::process::Command::new(env!("CARGO_BIN_EXE_vcrawl"))
        .args(["migrate", "--print-schema", "--contract"])
        .output()
        .expect("run vcrawl");
    assert!(
        !bad.status.success(),
        "--print-schema with --contract is refused"
    );
}

/// A migration numbered below the highest one (1102 after 1906) is not covered by the
/// highest-number schema label: the image's full set is compared with the database's.
#[test]
fn missing_migrations_sees_an_out_of_order_gap() {
    let all = migration_versions();
    assert!(all.contains(&1102) && all.contains(&1401) && all.contains(&1906));
    assert!(missing_migrations(&all).is_empty());
    let without: Vec<i64> = all
        .iter()
        .copied()
        .filter(|v| *v != 1102 && *v != 1401)
        .collect();
    // The highest version is applied, yet two migrations are missing.
    assert_eq!(without.iter().max(), Some(&schema_version()));
    assert_eq!(missing_migrations(&without), vec![1102, 1401]);
}

#[tokio::test]
async fn applied_versions_reads_the_database_set() {
    let pool = testkit::pg().await;
    assert_eq!(
        applied_versions(&pool).await.expect("applied"),
        migration_versions()
    );
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = 1102")
        .execute(&pool)
        .await
        .expect("forget 1102");
    let applied = applied_versions(&pool).await.expect("applied");
    assert_eq!(missing_migrations(&applied), vec![1102]);
}
