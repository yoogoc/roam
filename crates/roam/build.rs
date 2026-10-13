use std::{env, path::PathBuf};

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let resource = root.join("../../assets/packaging/roam.rc");
    let icon = root.join("../../assets/app-icon/roam.ico");
    println!("cargo:rerun-if-changed={}", resource.display());
    println!("cargo:rerun-if-changed={}", icon.display());

    // Build scripts run on the host, so check Cargo's target OS.
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let icon_definition = format!(
        "ROAM_ICON_PATH=\"{}\"",
        icon.to_string_lossy().replace('\\', "/")
    );
    embed_resource::compile_for(resource, ["roam"], [icon_definition])
        .manifest_required()
        .expect("failed to embed the Windows application icon");
}
