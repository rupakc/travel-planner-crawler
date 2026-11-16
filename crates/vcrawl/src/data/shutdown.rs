//! Shutdown signal shared by the long-running data-path roles.

use tokio::sync::watch;

/// Resolves on SIGINT or SIGTERM.
pub async fn signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// A watch channel that flips to `true` on [`signal`].
pub fn channel() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        signal().await;
        let _ = tx.send(true);
    });
    rx
}

/// Sleeps for `d` unless shutdown is requested first. Returns `true` when shutting down.
pub async fn sleep_or_stop(stop: &mut watch::Receiver<bool>, d: std::time::Duration) -> bool {
    if *stop.borrow() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(d) => *stop.borrow(),
        r = stop.changed() => r.is_err() || *stop.borrow(),
    }
}
