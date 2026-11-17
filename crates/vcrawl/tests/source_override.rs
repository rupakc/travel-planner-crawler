//! Admin decisions on a source (`vcrawl admin source disable|approve`) survive re-seeding:
//! the scheduler's upsert copies `enabled` from the spec only for sources nobody decided on.

use vc_core::sources::SourceSpec;
use vcrawl::admin::source;

fn spec(id: &str, extra: &str) -> SourceSpec {
    vc_sources::parse_spec(&format!(
        "id: {id}\nname: test\ncategory: test\nsource_class: official\n\
         licence: {{class: open, attribution: test}}\nmethod: html\ncadence: daily\n\
         seeds: [\"https://{id}.example/\"]\n{extra}"
    ))
    .expect("spec")
}

async fn state(pool: &sqlx::PgPool, id: &str) -> (bool, Option<bool>) {
    sqlx::query_as("SELECT enabled, enabled_override FROM sources WHERE source_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("row")
}

#[tokio::test]
async fn disabled_source_stays_disabled_when_its_spec_changes() {
    let pool = testkit::pg().await;
    vc_sources::seed_sources(&pool, &[spec("ops-a", "")], true)
        .await
        .unwrap();
    assert_eq!(state(&pool, "ops-a").await, (true, None));

    source::disable(&pool, "ops-a").await.unwrap();
    assert_eq!(state(&pool, "ops-a").await, (false, Some(false)));

    // The yaml changes (and still says enabled): the reseed rewrites the row, not the decision.
    let rep = vc_sources::seed_sources(&pool, &[spec("ops-a", "min_delay_s: 30\n")], true)
        .await
        .unwrap();
    assert_eq!(rep.updated, 1, "the spec change is applied");
    assert_eq!(state(&pool, "ops-a").await, (false, Some(false)));
    let rows = source::list(&pool).await.unwrap();
    assert_eq!(rows[0].enabled_override, Some(false));

    // Approve: enabled, and it stays enabled even if the yaml later says `enabled: false`.
    source::approve(&pool, "ops-a", "me").await.unwrap();
    vc_sources::seed_sources(&pool, &[spec("ops-a", "enabled: false\n")], true)
        .await
        .unwrap();
    assert_eq!(state(&pool, "ops-a").await, (true, Some(true)));

    // A licence that turns restricted still wins over an approval (never ingested).
    let restricted = vc_sources::parse_spec(
        "id: ops-a\nname: test\ncategory: test\nsource_class: official\n\
         licence: {class: restricted, attribution: test}\nmethod: html\ncadence: daily\n\
         seeds: [\"https://ops-a.example/\"]\nenabled: false\n",
    )
    .expect("spec");
    vc_sources::seed_sources(&pool, &[restricted], true)
        .await
        .unwrap();
    assert_eq!(state(&pool, "ops-a").await.0, false);
}

#[tokio::test]
async fn sources_nobody_decided_on_follow_their_spec() {
    let pool = testkit::pg().await;
    vc_sources::seed_sources(&pool, &[spec("ops-b", "")], true)
        .await
        .unwrap();
    vc_sources::seed_sources(&pool, &[spec("ops-b", "enabled: false\n")], true)
        .await
        .unwrap();
    assert_eq!(state(&pool, "ops-b").await, (false, None));
    vc_sources::seed_sources(&pool, &[spec("ops-b", "")], true)
        .await
        .unwrap();
    assert_eq!(state(&pool, "ops-b").await, (true, None));
}

/// The scheduler never seeds a source an admin disabled, even when its spec gains new seeds.
#[tokio::test]
async fn scheduler_does_not_seed_an_admin_disabled_source() {
    let pool = testkit::pg().await;
    let cfg = std::sync::Arc::new(vc_config::Config::default());
    let frontier = std::sync::Arc::new(vc_frontier::PgFrontier::new(
        pool.clone(),
        cfg.frontier.clone(),
    ));
    let sched = vcrawl::roles::scheduler::Scheduler::new(cfg, pool.clone(), frontier);
    sched.pass(&[spec("ops-c", "")]).await.unwrap();
    source::disable(&pool, "ops-c").await.unwrap();

    let changed = vc_sources::parse_spec(
        "id: ops-c\nname: test\ncategory: test\nsource_class: official\n\
         licence: {class: open, attribution: test}\nmethod: html\ncadence: daily\n\
         seeds: [\"https://ops-c.example/\", \"https://ops-c.example/new\"]\n",
    )
    .expect("spec");
    let rep = sched.pass(&[changed]).await.unwrap();
    assert_eq!((rep.sources.updated, rep.enqueued), (1, 0));
    let states: Vec<String> = sqlx::query_scalar("SELECT state FROM url_state ORDER BY state")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        states,
        ["disabled"],
        "only the old seed, parked by the disable"
    );
    assert_eq!(state(&pool, "ops-c").await, (false, Some(false)));
}
