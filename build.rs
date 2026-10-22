fn main() {
    // GPUI's Windows backend loads icon resource 1 for the window and taskbar; no-op on other targets.
    embed_resource::compile("assets/floki.rc", embed_resource::NONE).manifest_optional().unwrap();
    println!("cargo:rerun-if-changed=assets/floki.ico");
}
