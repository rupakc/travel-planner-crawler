//! Enables the Chromium integration tests (`cfg(vc_render_it)`) in CI (`CI` is set by GitHub
//! Actions) or when `VC_RENDER_IT` is set; elsewhere they are `#[ignore]`d.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(vc_render_it)");
    println!("cargo::rerun-if-env-changed=CI");
    println!("cargo::rerun-if-env-changed=VC_RENDER_IT");
    let on = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty() && v != "0" && v != "false");
    if on("CI") || on("VC_RENDER_IT") {
        println!("cargo::rustc-cfg=vc_render_it");
    }
}
