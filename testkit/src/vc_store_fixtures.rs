//! Fixtures for `vc-store` tests (owned by the vc-store task; add items here, never edit lib.rs).
//!
//! [`fake_gcs()`] starts one `fsouza/fake-gcs-server` container per test process (HTTP, memory
//! backend) with the bucket [`FAKE_GCS_BUCKET`]. Containers carry the same PID labels as the
//! Postgres container, so the orphan reaper in [`crate::pg`] removes them after the process.

use std::sync::OnceLock;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

/// The bucket that exists in the fake server.
pub const FAKE_GCS_BUCKET: &str = "vc-test";

const IMAGE: &str = "fsouza/fake-gcs-server";
const TAG: &str = "1.56.1";

/// A running fake-gcs-server.
pub struct FakeGcs {
    /// `http://host:port`.
    pub endpoint: String,
    _container: Option<Container<GenericImage>>,
}

impl std::fmt::Debug for FakeGcs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeGcs")
            .field("endpoint", &self.endpoint)
            .finish_non_exhaustive()
    }
}

static FAKE_GCS: OnceLock<FakeGcs> = OnceLock::new();

/// The process-wide fake GCS server (started on first use; `VC_TEST_GCS_ENDPOINT` uses an
/// existing one, which must already have the bucket [`FAKE_GCS_BUCKET`]).
pub fn fake_gcs() -> &'static FakeGcs {
    FAKE_GCS.get_or_init(|| {
        std::thread::spawn(start)
            .join()
            .expect("start fake-gcs-server")
    })
}

fn start() -> FakeGcs {
    if let Ok(endpoint) = std::env::var("VC_TEST_GCS_ENDPOINT") {
        return FakeGcs {
            endpoint,
            _container: None,
        };
    }
    let container = GenericImage::new(IMAGE, TAG)
        .with_exposed_port(4443.tcp())
        .with_wait_for(WaitFor::message_on_either_std("server started at"))
        .with_label("io.voyager.testkit", "1")
        .with_label("io.voyager.testkit.pid", std::process::id().to_string())
        // `-data` seeds buckets from directories: this creates the bucket.
        .with_copy_to(format!("/data/{FAKE_GCS_BUCKET}/.keep"), b"keep".to_vec())
        .with_cmd([
            "-scheme", "http", "-port", "4443", "-backend", "memory", "-data", "/data",
        ])
        .start()
        .expect("start fsouza/fake-gcs-server (is Docker running?)");
    let host = container.get_host().expect("container host").to_string();
    let port = container.get_host_port_ipv4(4443).expect("mapped port");
    FakeGcs {
        endpoint: format!("http://{host}:{port}"),
        _container: Some(container),
    }
}
