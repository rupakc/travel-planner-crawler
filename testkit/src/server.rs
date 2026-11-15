//! A tiny helper that serves an axum router on a local port for the fake servers.

use std::net::SocketAddr;

use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// A running fake server. Dropping it shuts the server down.
#[derive(Debug)]
pub struct FakeServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl FakeServer {
    /// Serves `router` on `127.0.0.1:<random port>`.
    pub async fn spawn(router: Router) -> FakeServer {
        let l = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        FakeServer::spawn_on(l, router)
    }

    /// Serves `router` on an already bound listener.
    pub fn spawn_on(listener: TcpListener, router: Router) -> FakeServer {
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = rx.await;
                })
                .await;
        });
        FakeServer {
            addr,
            shutdown: Some(tx),
            task: Some(task),
        }
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// `http://<addr>`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Shuts down and waits for the server task.
    pub async fn shutdown(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.task.take() {
            let _ = t.await;
        }
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}
