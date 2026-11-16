//! Fixtures for `vc-render` tests (owned by the vc-render task; add items here, never edit lib.rs).
//!
//! [`chromium_cdp_url()`] starts one pinned `chromedp/headless-shell` container per test process
//! with the production settings: `deploy/chromium/run.sh` as the entrypoint, Chrome's sandbox on
//! under `deploy/chromium/seccomp.json`, non-root, read-only root filesystem, all capabilities
//! dropped. Chromium never needs the network in these tests: every request is answered by the
//! renderer through CDP `Fetch` interception from a fake web.

use std::sync::OnceLock;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

/// The image, pinned by digest. Must equal the `chromium` service in `deploy/compose.yaml`
/// (a vc-render test checks this).
pub const CHROMIUM_IMAGE: &str = "chromedp/headless-shell";
/// Tag + digest of [`CHROMIUM_IMAGE`].
pub const CHROMIUM_TAG: &str =
    "158.0.8127.18@sha256:830fa77e0602cb42e5cb2cbd9237ba27b649ad69d5fb82b54713aac8b7ab4f29";

/// The container entrypoint script (`deploy/chromium/run.sh`).
pub const CHROMIUM_RUN_SH: &str = include_str!("../../deploy/chromium/run.sh");
/// Chrome's seccomp profile (`deploy/chromium/seccomp.json`).
pub const CHROMIUM_SECCOMP: &str = include_str!("../../deploy/chromium/seccomp.json");

const LABEL: &str = "io.voyager.testkit";
const LABEL_PID: &str = "io.voyager.testkit.pid";

struct Chromium {
    cdp_url: String,
    _container: Container<GenericImage>,
}

static CHROMIUM: OnceLock<Chromium> = OnceLock::new();

/// The CDP URL (`http://127.0.0.1:<port>`) of the per-process Chromium container, started on
/// first use. Set `VC_TEST_CHROMIUM_URL` to use an already running one instead.
pub fn chromium_cdp_url() -> String {
    if let Ok(u) = std::env::var("VC_TEST_CHROMIUM_URL") {
        return u;
    }
    CHROMIUM
        .get_or_init(|| {
            std::thread::spawn(start)
                .join()
                .expect("start chromedp/headless-shell")
        })
        .cdp_url
        .clone()
}

fn start() -> Chromium {
    reap_orphans();
    // Docker's API wants the seccomp profile inline (the CLI reads the file).
    let seccomp: String = CHROMIUM_SECCOMP
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let container = GenericImage::new(CHROMIUM_IMAGE, CHROMIUM_TAG)
        .with_entrypoint("/bin/sh")
        .with_exposed_port(9222.tcp())
        .with_wait_for(WaitFor::message_on_stderr("DevTools listening on"))
        .with_cmd(["-c", CHROMIUM_RUN_SH])
        .with_user("1000:1000")
        .with_readonly_rootfs(true)
        .with_cap_drop("ALL")
        .with_security_opt(format!("seccomp={seccomp}"))
        .with_security_opt("no-new-privileges:true")
        .with_shm_size(512 * 1024 * 1024)
        .with_env_var("HOME", "/tmp")
        .with_host_config_modifier(|hc| {
            let mut tmpfs = std::collections::HashMap::new();
            tmpfs.insert(
                "/tmp".to_string(),
                "size=512m,uid=1000,gid=1000".to_string(),
            );
            hc.tmpfs = Some(tmpfs);
        })
        .with_label(LABEL, "1")
        .with_label(LABEL_PID, std::process::id().to_string())
        .start()
        .expect("start chromedp/headless-shell (is Docker running?)");
    let host = container.get_host().expect("container host").to_string();
    let host = if host == "localhost" {
        "127.0.0.1".to_string()
    } else {
        host
    };
    let port = container.get_host_port_ipv4(9222).expect("mapped CDP port");
    // socat starts with Chromium; wait until the forwarded endpoint answers.
    for _ in 0..100 {
        if std::net::TcpStream::connect((host.as_str(), port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Chromium {
        cdp_url: format!("http://{host}:{port}"),
        _container: container,
    }
}

/// Removes testkit containers left behind by test processes that no longer exist. Best effort.
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
