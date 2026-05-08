//! Build script for Okiro.
//!
//! Runs the React/Vite UI build under `ui/` so that `ui/dist/` is present
//! when `rust-embed` picks it up to bake into the binary. The UI tree is
//! declared in `rerun-if-changed` so a plain `cargo build` with no UI
//! changes is effectively free (it still invokes `npm ci` but that's a
//! cache hit once `ui/node_modules` is populated).
//!
//! The build is skipped entirely when `OKIRO_SKIP_UI_BUILD=1`, which is
//! useful in local Rust-only iteration where you're running `vite dev` in
//! a separate terminal and don't want cargo to rebuild the bundle.
//!
//! If `npm` or `node` are not on PATH we fail loudly with an actionable
//! message. This is a hard requirement; we don't ship a fallback.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let ui_dir = manifest_dir.join("ui");
    let dist_dir = ui_dir.join("dist");

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
    println!("cargo:rerun-if-env-changed=OKIRO_SKIP_UI_BUILD");

    if std::env::var_os("OKIRO_SKIP_UI_BUILD").is_some() {
        // Still require `dist/` to exist so rust-embed doesn't fail. The
        // developer owns producing it via `npm run build` in the ui dir.
        if !dist_dir.exists() {
            println!(
                "cargo:warning=OKIRO_SKIP_UI_BUILD is set but {} does not exist; the binary will be missing the UI.",
                dist_dir.display()
            );
        }
        return;
    }

    // Fail fast if the toolchain is missing. The message has to be useful
    // because cargo squashes most of build.rs output unless there's an
    // error.
    if which("npm").is_none() {
        panic!("`npm` not found on PATH. Install Node.js (includes npm) and retry `cargo build`.");
    }
    if which("node").is_none() {
        panic!("`node` not found on PATH. Install Node.js and retry `cargo build`.");
    }

    // Use `npm ci` when a lockfile is present for reproducibility, else
    // `npm install`. First build seeds node_modules; subsequent builds are
    // no-ops when nothing in package.json changed (npm caches aggressively).
    let lock = ui_dir.join("package-lock.json");
    let install_args: &[&str] = if lock.exists() {
        &["ci", "--no-audit", "--no-fund", "--loglevel=error"]
    } else {
        &["install", "--no-audit", "--no-fund", "--loglevel=error"]
    };

    run("npm", install_args, &ui_dir);
    run("npm", &["run", "build", "--silent"], &ui_dir);

    if !dist_dir.join("index.html").exists() {
        panic!(
            "UI build completed but {} is missing. Check `ui/vite.config.ts` build output settings.",
            dist_dir.join("index.html").display()
        );
    }
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
