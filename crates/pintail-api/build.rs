use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    let crate_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("crate directory is available"));
    let dashboard_dir = crate_dir.join("../../packages/dashboard");
    let output_dir = dashboard_dir.join(".output/public");

    for input in [
        "app",
        "public",
        "components.json",
        "nuxt.config.ts",
        "package.json",
        "bun.lock",
        "tsconfig.json",
    ] {
        println!(
            "cargo::rerun-if-changed={}",
            dashboard_dir.join(input).display()
        );
    }
    println!("cargo::rerun-if-env-changed=PINTAIL_DASHBOARD_PREBUILT");

    if env::var_os("PINTAIL_DASHBOARD_PREBUILT").is_none() {
        generate_dashboard(&dashboard_dir);
    }
    assert_dashboard(&output_dir);
}

fn generate_dashboard(dashboard_dir: &Path) {
    let status = Command::new("bun")
        .args(["run", "generate"])
        .current_dir(dashboard_dir)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "failed to start Bun ({error}); run `bun install` in {}",
                dashboard_dir.display()
            )
        });
    assert!(status.success(), "Bun failed to generate the dashboard");
}

fn assert_dashboard(output_dir: &Path) {
    assert!(
        output_dir.join("index.html").is_file(),
        "dashboard output is missing at {}; run `bun install && bun run generate`",
        output_dir.display()
    );
}
