//! Render re-queue (spec §5.4, `render: auto`): an HTTP capture that vc-parse flags
//! `needs_render` puts its URL back in the frontier in `render` mode, due now, under normal
//! politeness, at most once per content hash, and only for `auto` sources. E2E-independent:
//! the real processor, Postgres frontier and raw-store commit path.

#[path = "processor_support.rs"]
mod support;

use chrono::{DateTime, Utc};
use sha2::Digest as _;
use sqlx::PgPool;
use support::*;
use vc_core::CanonicalUrl;
use vc_core::frontier::{Frontier, NewUrl};
use vc_core::media::FetchMode;
use vc_frontier::PgFrontier;

/// An SPA shell: scripts, an app root, no text.
const SHELL: &str = r#"<!doctype html><html><head><title>Attractions</title>
<script src="/static/app.js"></script></head><body><div id="root"></div></body></html>"#;
/// The same app after a deploy (new bundle name): a different content hash.
const SHELL_V2: &str = r#"<!doctype html><html><head><title>Attractions</title>
<script src="/static/app.4f2a.js"></script></head><body><div id="root"></div></body></html>"#;

fn article() -> String {
    format!(
        "<!doctype html><html><head><title>Lisbon</title><script src=\"/a.js\"></script></head>\
         <body><div id=\"root\"><main><p>{}</p></main></div></body></html>",
        "Lisbon's trams climb the old hills past miradouros and tiled facades. ".repeat(8)
    )
}

/// Seeds a source like the `blog-test` fixture, with its own id, host and render mode.
async fn seed_source(pool: &PgPool, id: &str, render: &str) {
    let yaml = fixture("sources/blog-test.yaml")
        .replace("id: blog-test", &format!("id: {id}"))
        .replace(
            "https://blog.example.org/",
            &format!("https://{id}.example.org/"),
        )
        .replace("render: never", &format!("render: {render}"));
    let spec = vc_sources::spec::parse_spec(&yaml).expect("spec");
    vc_sources::seed::seed_sources(pool, &[spec], false)
        .await
        .expect("seed");
}

async fn rig_with(requeue: bool) -> Rig {
    let r = rig(
        &[],
        RigOpts {
            render_requeue: requeue,
            ..RigOpts::default()
        },
    )
    .await;
    for (id, mode) in [
        ("spa-auto", "auto"),
        ("spa-never", "never"),
        ("spa-always", "always"),
    ] {
        seed_source(&r.pool, id, mode).await;
    }
    r
}

/// Enqueues `url` (HTTP mode) and moves it to its recrawl time a week out, as after a fetch.
async fn crawled(pool: &PgPool, url: &str, source: &str) -> [u8; 16] {
    let u = CanonicalUrl::parse(url).unwrap();
    let mut n = NewUrl::new(u.clone());
    n.source_id = Some(vc_core::SourceId(source.into()));
    let f = PgFrontier::new(pool.clone(), vc_config::FrontierConfig::default());
    assert_eq!(f.enqueue(&[n]).await.unwrap(), 1);
    sqlx::query(
        "UPDATE url_state SET next_fetch_at = now() + interval '7 days' WHERE url_key = $1",
    )
    .bind(u.key().as_bytes().as_slice())
    .execute(pool)
    .await
    .unwrap();
    *u.key().as_bytes()
}

#[derive(Debug, PartialEq)]
struct Row {
    mode: String,
    due_now: bool,
    refetch_now: bool,
    hash: Option<Vec<u8>>,
}

async fn row(pool: &PgPool, key: &[u8; 16]) -> Row {
    let (mode, due_now, refetch_now, hash): (String, bool, bool, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT fetch_mode, next_fetch_at <= now(), coalesce(refetch_at <= now(), false),
                    render_requested_hash
             FROM url_state WHERE url_key = $1",
    )
    .bind(key.as_slice())
    .fetch_one(pool)
    .await
    .unwrap();
    Row {
        mode,
        due_now,
        refetch_now,
        hash,
    }
}

fn sha(body: &str) -> Option<Vec<u8>> {
    Some(sha2::Sha256::digest(body.as_bytes()).to_vec())
}

async fn process(r: &Rig, cap: Cap<'_>) {
    commit(&r.pool, &r.blobs, cap).await;
    assert_eq!(r.proc.tick(10).await.unwrap(), 1);
}

fn http_row(due_now: bool) -> Row {
    Row {
        mode: "http".into(),
        due_now,
        refetch_now: false,
        hash: None,
    }
}

#[tokio::test]
async fn auto_spa_shell_is_queued_for_render_once_per_hash() {
    let r = rig_with(true).await;
    let url = "https://spa-auto.example.org/attractions";
    let key = crawled(&r.pool, url, "spa-auto").await;
    let pkey = CanonicalUrl::parse(url).unwrap().politeness_key().0;
    let keys_before: (DateTime<Utc>, i32) =
        sqlx::query_as("SELECT next_allowed_at, used_today FROM politeness_keys WHERE key = $1")
            .bind(&pkey)
            .fetch_one(&r.pool)
            .await
            .unwrap();

    process(&r, Cap::html(url, "spa-auto", SHELL, at(0))).await;
    assert_eq!(
        row(&r.pool, &key).await,
        Row {
            mode: "render".into(),
            due_now: true,
            refetch_now: false,
            hash: sha(SHELL),
        }
    );
    // Normal politeness: the key's pacing and budget are untouched; the render fetch is an
    // ordinary claim, and that claim leases the URL in render mode.
    let keys_after: (DateTime<Utc>, i32) =
        sqlx::query_as("SELECT next_allowed_at, used_today FROM politeness_keys WHERE key = $1")
            .bind(&pkey)
            .fetch_one(&r.pool)
            .await
            .unwrap();
    assert_eq!(keys_before, keys_after);
    let f = PgFrontier::new(r.pool.clone(), vc_config::FrontierConfig::default());
    sqlx::query("UPDATE politeness_keys SET next_allowed_at = now() + interval '1 hour'")
        .execute(&r.pool)
        .await
        .unwrap();
    assert!(
        f.claim(&[0], 5).await.unwrap().is_none(),
        "a key that is not ready is not claimed for the render"
    );
    sqlx::query("UPDATE politeness_keys SET next_allowed_at = now() - interval '1 second'")
        .execute(&r.pool)
        .await
        .unwrap();
    let claim = f.claim(&[0], 5).await.unwrap().expect("claim");
    assert_eq!(claim.urls.len(), 1);
    assert_eq!(claim.urls[0].fetch_mode, FetchMode::Render);
    // The fetch completes (back to due, recrawl a week out) and the URL stays a render URL.
    sqlx::query(
        "UPDATE url_state SET state = 'due', lease_epoch = NULL,
                next_fetch_at = now() + interval '7 days' WHERE url_key = $1",
    )
    .bind(key.as_slice())
    .execute(&r.pool)
    .await
    .unwrap();

    // The same capture redelivered, or the same shell fetched over HTTP again (e.g. the URL
    // was put back on HTTP): never a second request for that hash.
    sqlx::query("UPDATE url_state SET fetch_mode = 'http' WHERE url_key = $1")
        .bind(key.as_slice())
        .execute(&r.pool)
        .await
        .unwrap();
    process(&r, Cap::html(url, "spa-auto", SHELL, at(10))).await;
    assert_eq!(
        row(&r.pool, &key).await,
        Row {
            mode: "http".into(),
            due_now: false,
            refetch_now: false,
            hash: sha(SHELL),
        },
        "one content hash re-queues at most once"
    );
    // A new shell (a different hash) is queued again.
    process(&r, Cap::html(url, "spa-auto", SHELL_V2, at(20))).await;
    assert_eq!(
        row(&r.pool, &key).await,
        Row {
            mode: "render".into(),
            due_now: true,
            refetch_now: false,
            hash: sha(SHELL_V2),
        }
    );
}

#[tokio::test]
async fn render_capture_or_url_already_in_render_mode_is_not_queued_again() {
    let r = rig_with(true).await;
    let url = "https://spa-auto.example.org/still-a-shell";
    let key = crawled(&r.pool, url, "spa-auto").await;
    // A render capture that still looks like a shell: rendering is what we asked for already.
    process(
        &r,
        Cap {
            fetch_mode: FetchMode::Render,
            ..Cap::html(url, "spa-auto", SHELL, at(0))
        },
    )
    .await;
    assert_eq!(row(&r.pool, &key).await, http_row(false));
    // A URL already in render mode (e.g. an HTTP fallback while Chromium is away) stays put.
    sqlx::query("UPDATE url_state SET fetch_mode = 'render' WHERE url_key = $1")
        .bind(key.as_slice())
        .execute(&r.pool)
        .await
        .unwrap();
    process(&r, Cap::html(url, "spa-auto", SHELL, at(5))).await;
    assert_eq!(
        row(&r.pool, &key).await,
        Row {
            mode: "render".into(),
            ..http_row(false)
        }
    );
}

#[tokio::test]
async fn only_auto_sources_and_flagged_pages_are_queued() {
    let r = rig_with(true).await;
    // `never` and `always` sources: no re-queue (always-source URLs are seeded in render mode).
    for src in ["spa-never", "spa-always"] {
        let url = format!("https://{src}.example.org/attractions");
        let key = crawled(&r.pool, &url, src).await;
        process(&r, Cap::html(&url, src, SHELL, at(0))).await;
        assert_eq!(row(&r.pool, &key).await, http_row(false), "{src}");
    }
    // An `auto` page with enough text is not flagged.
    let url = "https://spa-auto.example.org/lisbon";
    let key = crawled(&r.pool, url, "spa-auto").await;
    let body = article();
    process(&r, Cap::html(url, "spa-auto", &body, at(1))).await;
    assert_eq!(row(&r.pool, &key).await, http_row(false));
}

#[tokio::test]
async fn rendering_off_never_requeues() {
    let r = rig_with(false).await;
    let url = "https://spa-auto.example.org/attractions";
    let key = crawled(&r.pool, url, "spa-auto").await;
    process(&r, Cap::html(url, "spa-auto", SHELL, at(0))).await;
    assert_eq!(row(&r.pool, &key).await, http_row(false));
}

#[tokio::test]
async fn a_url_being_fetched_is_queued_through_refetch_at() {
    let r = rig_with(true).await;
    let url = "https://spa-auto.example.org/attractions";
    let key = crawled(&r.pool, url, "spa-auto").await;
    // A later fetch of the URL is running: the running lease is not disturbed; the render
    // becomes due the moment the URL returns to `due` (migration 1101's trigger).
    sqlx::query("UPDATE url_state SET state = 'leased', lease_epoch = 7 WHERE url_key = $1")
        .bind(key.as_slice())
        .execute(&r.pool)
        .await
        .unwrap();
    process(&r, Cap::html(url, "spa-auto", SHELL, at(0))).await;
    assert_eq!(
        row(&r.pool, &key).await,
        Row {
            mode: "render".into(),
            due_now: false,
            refetch_now: true,
            hash: sha(SHELL),
        }
    );
    sqlx::query("UPDATE url_state SET state = 'due', lease_epoch = NULL WHERE url_key = $1")
        .bind(key.as_slice())
        .execute(&r.pool)
        .await
        .unwrap();
    let back = row(&r.pool, &key).await;
    assert!(back.due_now && !back.refetch_now, "{back:?}");
    assert_eq!(back.mode, "render");
}
