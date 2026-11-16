//! `fake_web`: serves the seven scripted sites (spec §12) for E2E.
//!
//! ```text
//! fake_web [--listen 0.0.0.0:80] [--cycle-secs N] [--slow-latency-ms 2000]
//!          [--slow-crawl-delay-s 10] [--content-change-cycle 1] [--robots-change-cycle 2]
//! ```
//!
//! Every flag also reads `FAKE_WEB_<FLAG>` (e.g. `FAKE_WEB_LISTEN`). Each request is printed to
//! stdout as one JSON line (the frozen request-log schema), so `docker compose logs fake-web`
//! shows the log; `GET /__admin/log` and `GET /__admin/snapshot` serve it to the oracle.
//! With `--cycle-secs N > 0` the cycle advances every N seconds; otherwise only
//! `POST /__admin/cycle` moves it.

use std::sync::Arc;
use std::time::Duration;

use testkit::fake_web::{FakeWeb, sites};
use testkit::server::FakeServer;

fn arg(name: &str) -> Option<String> {
    let flag = format!("--{name}");
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == flag {
            return it.next();
        }
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v.to_string());
        }
    }
    std::env::var(format!(
        "FAKE_WEB_{}",
        name.replace('-', "_").to_ascii_uppercase()
    ))
    .ok()
}

fn num(name: &str, default: u64) -> u64 {
    arg(name)
        .map(|v| {
            v.parse()
                .unwrap_or_else(|_| panic!("--{name} must be a number"))
        })
        .unwrap_or(default)
}

#[tokio::main]
async fn main() {
    let listen = arg("listen").unwrap_or_else(|| "0.0.0.0:80".into());
    let d = sites::SiteOptions::default();
    let opts = sites::SiteOptions {
        content_change_cycle: num("content-change-cycle", d.content_change_cycle as u64) as usize,
        robots_change_cycle: num("robots-change-cycle", d.robots_change_cycle as u64) as usize,
        slow_crawl_delay_s: num("slow-crawl-delay-s", u64::from(d.slow_crawl_delay_s)) as u32,
        slow_latency: Duration::from_millis(num(
            "slow-latency-ms",
            d.slow_latency.as_millis() as u64,
        )),
        slow_retry_after_s: num("slow-retry-after-s", u64::from(d.slow_retry_after_s)) as u32,
    };
    let cycle_secs = num("cycle-secs", 0);

    let web = Arc::new(FakeWeb::with_sites(sites::all(&opts)));
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| panic!("bind {listen}: {e}"));
    let _server = FakeServer::spawn_on(listener, web.clone().router());
    eprintln!(
        "fake_web listening on {listen}; sites: {}",
        web.hosts().join(", ")
    );

    if cycle_secs > 0 {
        let w = web.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(cycle_secs)).await;
                let c = w.next_cycle();
                eprintln!("fake_web cycle {c}");
            }
        });
    }

    // Print new log entries as JSON lines.
    let log = web.log();
    let mut printed = 0usize;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                let n = log.len();
                if n < printed {
                    printed = 0; // reset via /__admin/reset
                }
                if n > printed {
                    let mut all = log.entries();
                    all.sort_by_key(|e| e.ended_at);
                    for e in all.iter().skip(printed) {
                        println!("{}", serde_json::to_string(e).expect("entry serialises"));
                    }
                    printed = n;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
}
