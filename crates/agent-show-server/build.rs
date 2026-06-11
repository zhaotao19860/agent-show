use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=../../web/package.json");
    println!("cargo:rerun-if-changed=../../web/package-lock.json");
    println!("cargo:rerun-if-changed=../../web/index.html");
    println!("cargo:rerun-if-changed=../../web/src");
    println!("cargo:rerun-if-changed=../../web/vite.config.ts");
    println!("cargo:rerun-if-changed=../../web/tsconfig.json");
    println!("cargo:rerun-if-changed=../../web/tsconfig.node.json");
    println!("cargo:rerun-if-changed=../../web/tailwind.config.ts");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set"));
    let web_dir = manifest_dir.join("../../web");

    run_npm(&web_dir, &["ci", "--ignore-scripts"]);
    run_npm(&web_dir, &["run", "build"]);
}

fn run_npm(web_dir: &PathBuf, args: &[&str]) {
    let status = Command::new("npm")
        .args(args)
        .current_dir(web_dir)
        .status()
        .unwrap_or_else(|err| panic!("failed to run npm {}: {err}", args.join(" ")));

    if !status.success() {
        panic!("npm {} failed with status {status}", args.join(" "));
    }
}
