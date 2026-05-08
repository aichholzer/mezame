//! Build script for Okiro.
//!
//! Runs the React/Vite UI build under `$OUT_DIR/ui/` (copied from the
//! crate's `ui/` sources) so that `$OUT_DIR/ui/dist/` is present when
//! `rust-embed` picks it up to bake into the binary.
//!
//! Doing the build in `$OUT_DIR` is a hard crates.io requirement: `cargo
//! publish` verifies that `build.rs` does not modify the source
//! directory. Writing `node_modules/` or `dist/` into the crate's own
//! `ui/` would break that check.
//!
//! The UI tree is declared in `rerun-if-changed` so a plain `cargo build`
//! with no UI changes is a cache hit after the first `npm ci`.
//!
//! The build is skipped entirely when `OKIRO_SKIP_UI_BUILD=1`, which is
//! useful in local Rust-only iteration where you're running `vite dev` in
//! a separate terminal.
//!
//! If `npm` or `node` are not on PATH we fail loudly with an actionable
//! message. This is a hard requirement; we don't ship a fallback.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR must be set by cargo"));
    let ui_src = manifest_dir.join("ui");
    let ui_build = out_dir.join("ui");
    let dist_dir = ui_build.join("dist");

    // Invalidate on any UI source change. Anything outside this list does
    // not affect the embedded bundle.
    println!("cargo:rerun-if-changed=ui/package.json");
    println!("cargo:rerun-if-changed=ui/package-lock.json");
    println!("cargo:rerun-if-changed=ui/vite.config.ts");
    println!("cargo:rerun-if-changed=ui/tsconfig.json");
    println!("cargo:rerun-if-changed=ui/tsconfig.app.json");
    println!("cargo:rerun-if-changed=ui/tsconfig.node.json");
    println!("cargo:rerun-if-changed=ui/index.html");
    println!("cargo:rerun-if-changed=ui/src");
    println!("cargo:rerun-if-changed=ui/public");
    println!("cargo:rerun-if-changed=ui/components.json");
    println!("cargo:rerun-if-env-changed=OKIRO_SKIP_UI_BUILD");

    if std::env::var_os("OKIRO_SKIP_UI_BUILD").is_some() {
        // Leave an empty dist/ directory so rust-embed doesn't fail to
        // resolve the `$OUT_DIR/ui/dist/` folder. The binary will be
        // missing the UI; that's on the developer who set the flag.
        std::fs::create_dir_all(&dist_dir).unwrap_or_else(|e| {
            panic!("failed to create {}: {e}", dist_dir.display())
        });
        return;
    }

    // Fail fast if the toolchain is missing. The message has to be useful
    // because cargo squashes most of build.rs output unless there's an
    // error.
    if which("npm").is_none() {
        panic!("`npm` not found on PATH. Install Node.js (includes npm) and retry `cargo build` or `cargo install okiro`.");
    }
    if which("node").is_none() {
        panic!("`node` not found on PATH. Install Node.js and retry `cargo build` or `cargo install okiro`.");
    }

    // Mirror the source tree into OUT_DIR. Only the UI inputs, never
    // node_modules or dist from the source tree. node_modules/ is
    // populated by the subsequent `npm ci`, dist/ by `npm run build`.
    // We re-sync every build so edits flow through; the copy is cheap
    // compared to the npm install itself.
    sync_ui_sources(&ui_src, &ui_build);

    let lock = ui_build.join("package-lock.json");
    let install_args: &[&str] = if lock.exists() {
        &["ci", "--no-audit", "--no-fund", "--loglevel=error"]
    } else {
        &["install", "--no-audit", "--no-fund", "--loglevel=error"]
    };

    run("npm", install_args, &ui_build);
    run("npm", &["run", "build", "--silent"], &ui_build);

    if !dist_dir.join("index.html").exists() {
        panic!(
            "UI build completed but {} is missing. Check `ui/vite.config.ts` build output settings.",
            dist_dir.join("index.html").display()
        );
    }
}

/// Mirror the `ui/` source tree to `$OUT_DIR/ui/`, excluding
/// `node_modules/` and `dist/` so we never pollute the build directory
/// with anything we would not have checked into git.
fn sync_ui_sources(src: &Path, dst: &Path) {
    if src == dst {
        return;
    }
    std::fs::create_dir_all(dst).unwrap_or_else(|e| {
        panic!("failed to create {}: {e}", dst.display())
    });
    for entry in std::fs::read_dir(src).unwrap_or_else(|e| {
        panic!("failed to read {}: {e}", src.display())
    }) {
        let entry = entry.unwrap_or_else(|e| panic!("read_dir entry failed: {e}"));
        let name = entry.file_name();
        // Skip caches and build outputs. `node_modules` is the expensive
        // one; `dist` and `.*.tsbuildinfo` we also do not want to bring
        // across (they live in the source tree during local dev).
        if name == "node_modules" || name == "dist" {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        let ft = entry
            .file_type()
            .unwrap_or_else(|e| panic!("file_type for {}: {e}", src_path.display()));
        if ft.is_dir() {
            sync_ui_sources(&src_path, &dst_path);
        } else if ft.is_file() {
            // Skip TS incremental build info written next to tsconfig.
            if name.to_string_lossy().ends_with(".tsbuildinfo") {
                continue;
            }
            copy_if_changed(&src_path, &dst_path);
        }
        // Symlinks and others: ignore. The ui/ tree does not use them.
    }
}

/// Copy only when the source is newer than the destination. Keeps npm's
/// caches happy by not touching files that did not actually change.
fn copy_if_changed(src: &Path, dst: &Path) {
    let src_meta = std::fs::metadata(src)
        .unwrap_or_else(|e| panic!("stat {}: {e}", src.display()));
    if let Ok(dst_meta) = std::fs::metadata(dst) {
        let (Ok(s), Ok(d)) = (src_meta.modified(), dst_meta.modified()) else {
            copy_file(src, dst);
            return;
        };
        if d >= s {
            return;
        }
    }
    copy_file(src, dst);
}

fn copy_file(src: &Path, dst: &Path) {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).unwrap_or_else(|e| {
            panic!("failed to create {}: {e}", parent.display())
        });
    }
    std::fs::copy(src, dst).unwrap_or_else(|e| {
        panic!("copy {} -> {} failed: {e}", src.display(), dst.display())
    });
}

fn run(cmd: &str, args: &[&str], cwd: &std::path::Path) {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(cwd)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn `{cmd}`: {e}"));
    if !status.success() {
        panic!("`{cmd} {}` failed in {}", args.join(" "), cwd.display());
    }
}

/// Tiny PATH lookup; avoids pulling in a dep for something this small.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for ext in ["", ".cmd", ".exe"] {
            let candidate = dir.join(format!("{name}{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
