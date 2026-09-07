//! The container recipe as text: the Dockerfile, compose.yaml and the
//! build-context allowlist, read from the repository at run time. The
//! image itself is built and run by `.github/workflows/container.yml`,
//! which needs Docker; these cases pin the shape of the files on every
//! `cargo test` so a slip is caught before that job runs.

use std::collections::BTreeSet;

fn repo_file(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The lines of `text` that are neither blank nor comments.
fn code_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .collect()
}

#[test]
fn every_from_line_is_pinned_to_an_image_index_digest() {
    // A tag is a moving reference; the digest of the multi-arch index is
    // not. The `# syntax=` frontend line was a third unpinned input and is
    // gone: the file uses no feature the built-in frontend lacks.
    let dockerfile = repo_file("Dockerfile");
    let froms: Vec<&str> = dockerfile
        .lines()
        .filter(|l| l.starts_with("FROM "))
        .collect();
    assert_eq!(froms.len(), 2, "two stages");
    for from in froms {
        let image = from.split_whitespace().nth(1).expect("an image reference");
        let (name, digest) = image
            .split_once("@sha256:")
            .unwrap_or_else(|| panic!("{from:?} carries no digest"));
        assert_eq!(digest.len(), 64, "{from:?}: a sha256 is 64 hex characters");
        assert!(
            digest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{from:?}: lowercase hex only"
        );
        // On the name half, not the whole reference, which always holds
        // the ':' of "@sha256:". The tag is what Dependabot refreshes the
        // digest against; a digest-only reference would be retargeted to
        // `latest`.
        let repo_and_tag = name.rsplit('/').next().unwrap_or(name);
        assert!(
            repo_and_tag.contains(':'),
            "{from:?} keeps its tag in front of the digest"
        );
    }
    assert!(
        !dockerfile.lines().any(|l| l.starts_with("# syntax=")),
        "no floating frontend image"
    );
}

#[test]
fn the_runtime_stage_drops_root_before_its_command() {
    // Requirement 20 criterion 14 (added 2026-09-07).
    let dockerfile = repo_file("Dockerfile");
    let all: Vec<&str> = dockerfile.lines().collect();
    let last_from = all
        .iter()
        .rposition(|l| l.starts_with("FROM "))
        .expect("a FROM line");
    let runtime: Vec<&str> = all[last_from..].to_vec();
    let user = runtime
        .iter()
        .position(|l| *l == "USER mezame")
        .expect("the runtime stage sets USER mezame");
    let cmd = runtime
        .iter()
        .position(|l| l.starts_with("CMD "))
        .expect("the runtime stage has a CMD");
    assert!(user < cmd, "USER precedes CMD");
    assert!(
        runtime.contains(&"ENV HOME=/home/mezame"),
        "HOME is set explicitly for the unprivileged user"
    );
}

#[test]
fn the_volume_is_the_declared_home_s_dot_mezame() {
    // Requirement 20 criterion 13: both services mount the one volume at
    // the path the image declares, which is the unprivileged user's home.
    let dockerfile = repo_file("Dockerfile");
    let home = dockerfile
        .lines()
        .find_map(|l| l.strip_prefix("ENV HOME="))
        .expect("ENV HOME");
    let volumes: Vec<&str> = dockerfile
        .lines()
        .filter(|l| l.starts_with("VOLUME "))
        .collect();
    assert_eq!(
        volumes,
        vec![format!("VOLUME [\"{home}/.mezame\"]").as_str()]
    );

    let compose = repo_file("compose.yaml");
    let mount = format!("- mezame-config:{home}/.mezame");
    assert_eq!(
        code_lines(&compose)
            .iter()
            .filter(|l| l.trim() == mount)
            .count(),
        2,
        "both services mount the volume at {home}/.mezame"
    );
    assert!(
        !code_lines(&compose)
            .iter()
            .any(|l| l.contains("/root/.mezame")),
        "no service still names the root path"
    );
}

#[test]
fn both_compose_services_are_hardened() {
    // Read-only root filesystem, every capability dropped, no privilege
    // escalation, on both services.
    let compose = repo_file("compose.yaml");
    let lines = code_lines(&compose);
    // The service blocks sit between the top-level `services:` and
    // `volumes:` keys, each opened by a two-space-indented name.
    let services_at = lines
        .iter()
        .position(|l| *l == "services:")
        .expect("a services key");
    let volumes_at = lines
        .iter()
        .position(|l| *l == "volumes:")
        .expect("a volumes key");
    let service_starts: Vec<usize> = (services_at + 1..volumes_at)
        .filter(|i| {
            let l = lines[*i];
            l.starts_with("  ") && !l.starts_with("   ") && l.ends_with(':')
        })
        .collect();
    let services: Vec<&str> = service_starts
        .iter()
        .map(|i| lines[*i].trim().trim_end_matches(':'))
        .collect();
    assert_eq!(
        services,
        vec!["mezame", "setup"],
        "the two services, in order"
    );
    for (n, start) in service_starts.iter().enumerate() {
        let end = service_starts.get(n + 1).copied().unwrap_or(volumes_at);
        let block = &lines[*start..end];
        for wanted in [
            "read_only: true",
            "cap_drop: [ALL]",
            "no-new-privileges:true",
        ] {
            assert!(
                block.iter().any(|l| l.contains(wanted)),
                "service {} lacks {wanted}",
                services[n]
            );
        }
    }
}

#[test]
fn the_image_declares_a_health_probe_on_the_exposed_port() {
    let dockerfile = repo_file("Dockerfile");
    let port = dockerfile
        .lines()
        .find_map(|l| l.strip_prefix("EXPOSE "))
        .expect("an EXPOSE line")
        .trim();
    let health: Vec<&str> = dockerfile
        .lines()
        .filter(|l| l.starts_with("HEALTHCHECK "))
        .collect();
    assert_eq!(health.len(), 1, "one HEALTHCHECK");
    // The instruction continues on the next line; join the two.
    let instruction = dockerfile
        .lines()
        .skip_while(|l| !l.starts_with("HEALTHCHECK "))
        .take(2)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        instruction.contains(&format!("http://127.0.0.1:{port}/")),
        "the probe fetches the exposed port: {instruction}"
    );

    let compose = repo_file("compose.yaml");
    let lines = code_lines(&compose);
    let setup = lines
        .iter()
        .position(|l| *l == "  setup:")
        .expect("the setup service");
    let health_at = lines[setup..]
        .iter()
        .position(|l| l.trim() == "healthcheck:")
        .expect("setup declares a healthcheck block");
    assert_eq!(
        lines[setup + health_at + 1].trim(),
        "disable: true",
        "the setup service switches the probe off"
    );
}

#[test]
fn the_build_context_is_an_allowlist_of_build_inputs() {
    // Everything out, six inputs in, and what `!ui/` lets back in taken
    // out again after it. A stray .env, the gitignored spec or a scanner
    // directory can no longer reach the builder layer.
    let ignore = repo_file(".dockerignore");
    let lines = code_lines(&ignore);
    assert_eq!(
        lines.first().copied(),
        Some("*"),
        "everything is excluded first"
    );
    let allowed: BTreeSet<&str> = lines.iter().filter_map(|l| l.strip_prefix('!')).collect();
    assert_eq!(
        allowed,
        BTreeSet::from([
            "Cargo.toml",
            "Cargo.lock",
            "build.rs",
            "src/",
            "benches/",
            "ui/"
        ]),
        "exactly the six build inputs are let back in"
    );
    let ui_at = lines.iter().position(|l| *l == "!ui/").expect("!ui/");
    for reexcluded in ["ui/node_modules/", "ui/dist/", "**/.env", "**/.env.*"] {
        let at = lines
            .iter()
            .position(|l| *l == reexcluded)
            .unwrap_or_else(|| panic!("{reexcluded} is re-excluded"));
        assert!(
            at > ui_at,
            "{reexcluded} comes after !ui/, or it has no effect"
        );
    }
}

#[test]
fn the_port_is_published_on_the_host_s_loopback_only() {
    // Requirement 20 criterion 13 as amended. "9510:9510" would publish the
    // unauthenticated port on every host interface, past ufw on Linux; the
    // compose header, the README and the change log all promise loopback
    // only, and nothing else pinned it.
    let compose = repo_file("compose.yaml");
    let lines = code_lines(&compose);
    let ports: Vec<usize> = (0..lines.len())
        .filter(|i| lines[*i].trim() == "ports:")
        .collect();
    assert_eq!(ports.len(), 1, "exactly one service publishes a port");
    let mappings: Vec<&str> = lines[ports[0] + 1..]
        .iter()
        .take_while(|l| l.trim_start().starts_with("- "))
        .map(|l| l.trim())
        .collect();
    assert_eq!(
        mappings,
        vec![r#"- "127.0.0.1:9510:9510""#],
        "the one mapping binds the host's loopback"
    );
    let setup = lines
        .iter()
        .position(|l| *l == "  setup:")
        .expect("the setup service");
    assert!(
        !lines[setup..].iter().any(|l| l.trim() == "ports:"),
        "the setup service publishes nothing"
    );
}
