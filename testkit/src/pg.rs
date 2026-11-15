//! Migrated Postgres databases for tests.
//!
//! One `pgvector/pgvector:pg17` container per test process (started on first use from a plain
//! thread, so it works from any tokio runtime). The first [`pg()`] call migrates a template
//! database; every call then clones it (`CREATE DATABASE … TEMPLATE …`) so each test gets an
//! isolated, migrated database in milliseconds.
//!
//! Set `VC_TEST_DATABASE_URL` (a superuser URL, e.g. `postgres://postgres:postgres@localhost:5432/postgres`)
//! to use an existing server instead of a container (CI service containers).
//!
//! testcontainers 0.28 has no reaper, and statics are never dropped, so containers are labelled
//! with the owning PID and every new test process removes containers whose owner is gone.

use std::sync::OnceLock;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use sqlx::{AssertSqlSafe, ConnectOptions, Connection, Executor, PgConnection};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};
use tokio::sync::{Mutex, OnceCell};

/// The workspace migrations (`migrations/`), embedded at compile time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

const IMAGE: &str = "pgvector/pgvector";
const TAG: &str = "pg17";
const LABEL: &str = "io.voyager.testkit";
const LABEL_PID: &str = "io.voyager.testkit.pid";

struct Server {
    admin: PgConnectOptions,
    _container: Option<Container<GenericImage>>,
}

static SERVER: OnceLock<Server> = OnceLock::new();
static TEMPLATE: OnceCell<String> = OnceCell::const_new();
static CLONE_LOCK: Mutex<()> = Mutex::const_new(());

fn server() -> &'static Server {
    SERVER.get_or_init(|| {
        std::thread::spawn(start_server)
            .join()
            .expect("start postgres")
    })
}

fn start_server() -> Server {
    if let Ok(url) = std::env::var("VC_TEST_DATABASE_URL") {
        let admin: PgConnectOptions = url.parse().expect("VC_TEST_DATABASE_URL is a postgres URL");
        return Server {
            admin,
            _container: None,
        };
    }
    reap_orphans();
    let container = GenericImage::new(IMAGE, TAG)
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_label(LABEL, "1")
        .with_label(LABEL_PID, std::process::id().to_string())
        .with_cmd([
            "postgres",
            "-c",
            "fsync=off",
            "-c",
            "synchronous_commit=off",
            "-c",
            "full_page_writes=off",
            "-c",
            "max_connections=500",
        ])
        .start()
        .expect("start pgvector/pgvector:pg17 (is Docker running?)");
    let host = container.get_host().expect("container host").to_string();
    let port = container.get_host_port_ipv4(5432).expect("mapped port");
    let admin = PgConnectOptions::new()
        .host(&host)
        .port(port)
        .username("postgres")
        .password("postgres")
        .database("postgres");
    Server {
        admin,
        _container: Some(container),
    }
}

/// Removes containers left behind by test processes that no longer exist. Best effort.
fn reap_orphans() {
    let out = std::process::Command::new("docker")
        .args([
            "ps",
            "-a",
            "--filter",
            &format!("label={LABEL}=1"),
            "--format",
        ])
        .arg(format!("{{{{.ID}}}} {{{{.Label \"{LABEL_PID}\"}}}}"))
        .output();
    let Ok(out) = out else { return };
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut it = line.split_whitespace();
        let (Some(id), Some(pid)) = (it.next(), it.next()) else {
            continue;
        };
        let alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .map(|s| s.success())
            .unwrap_or(true);
        if !alive {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", "-v", id])
                .output();
        }
    }
}

async fn admin_conn() -> PgConnection {
    let opts = server().admin.clone().disable_statement_logging();
    let mut last = None;
    // The postgres image restarts once after initdb; retry for up to 60 s.
    for _ in 0..120 {
        match PgConnection::connect_with(&opts).await {
            Ok(c) => return c,
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("cannot connect to test postgres: {last:?}");
}

fn unique_name(prefix: &str) -> String {
    format!(
        "{prefix}_{}_{}",
        std::process::id(),
        ulid::Ulid::generate().to_string().to_lowercase()
    )
}

async fn template() -> &'static str {
    TEMPLATE
        .get_or_init(|| async {
            let name = unique_name("vct_tpl");
            let mut admin = admin_conn().await;
            admin
                .execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
                .await
                .expect("create template");
            admin.close().await.ok();
            let opts = server().admin.clone().database(&name);
            let mut conn = PgConnection::connect_with(&opts)
                .await
                .expect("connect template");
            MIGRATOR
                .run(&mut conn)
                .await
                .expect("migrations apply on an empty database");
            conn.close().await.ok();
            name
        })
        .await
}

/// A fresh, migrated database and its connection options.
pub async fn pg_with_url() -> (PgPool, PgConnectOptions) {
    let tpl = template().await;
    let name = unique_name("vct");
    {
        let _g = CLONE_LOCK.lock().await;
        let mut admin = admin_conn().await;
        admin
            .execute(AssertSqlSafe(format!(
                "CREATE DATABASE {name} TEMPLATE {tpl}"
            )))
            .await
            .expect("clone template database");
        admin.close().await.ok();
    }
    let opts = server()
        .admin
        .clone()
        .database(&name)
        .disable_statement_logging();
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .acquire_timeout(Duration::from_secs(30))
        .connect_with(opts.clone())
        .await
        .expect("connect test database");
    (pool, opts)
}

/// A fresh, fully migrated database (isolated per call).
pub async fn pg() -> PgPool {
    pg_with_url().await.0
}
