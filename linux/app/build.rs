//! Compiles the Blueprint views in `ui/` to GtkBuilder XML in OUT_DIR.

use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=ui");
    let out = std::env::var("OUT_DIR").unwrap();
    let blps: Vec<_> = std::fs::read_dir("ui")
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "blp"))
        .collect();
    let ok = Command::new("blueprint-compiler")
        .args(["batch-compile", &out, "ui"])
        .args(&blps)
        .status()
        .expect("blueprint-compiler not found; install it to build the Linux app")
        .success();
    assert!(ok, "blueprint-compiler failed");
}
