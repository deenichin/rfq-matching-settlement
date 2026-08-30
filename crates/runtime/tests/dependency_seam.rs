//! S0 gate: the dependency direction of SPEC §13.1, checked as a whole graph.
//!
//! `rfq-core`'s own build script refuses to compile the engine if it declares any
//! dependency at all — that is the compile-time half, and it is the one that matters,
//! because a violation there silently falsifies everything the design claims about v2.
//! This test covers the rest of the graph, which cargo does not otherwise constrain:
//! custody must not learn about the runtime, and nothing may depend on the scenarios
//! binary.
//!
//! It reads the manifests rather than the compiled crates because the manifest is where
//! the violation would be introduced. A dependency that is declared but unused compiles
//! cleanly and is exactly the state this is looking for.

#![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]

use std::path::PathBuf;

/// Repository root, from this crate's manifest directory.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// Every dependency name declared by a crate, across all three dependency tables.
fn declared_dependencies(crate_dir: &str) -> Vec<String> {
    let manifest_path = workspace_root().join("crates").join(crate_dir).join("Cargo.toml");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|err| panic!("cannot read {}: {err}", manifest_path.display()));

    let mut names = Vec::new();
    let mut in_dependency_table = false;
    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let tail = header.rsplit('.').next().unwrap_or(header);
            in_dependency_table =
                matches!(tail, "dependencies" | "build-dependencies" | "dev-dependencies");
            // `[dependencies.rfq-chain]` names the dependency in the header itself.
            if !in_dependency_table
                && let Some((prefix, name)) = header.rsplit_once('.')
            {
                let table = prefix.rsplit('.').next().unwrap_or(prefix);
                if matches!(table, "dependencies" | "build-dependencies" | "dev-dependencies") {
                    names.push(name.trim().trim_matches('"').to_owned());
                }
            }
            continue;
        }
        if in_dependency_table
            && let Some((key, _)) = line.split_once('=')
        {
            let name = key.trim().trim_matches('"').split('.').next().unwrap_or("").trim();
            if !name.is_empty() {
                names.push(name.to_owned());
            }
        }
    }
    names
}

fn assert_dependencies_within(crate_dir: &str, permitted: &[&str], why: &str) {
    for name in declared_dependencies(crate_dir) {
        assert!(
            permitted.contains(&name.as_str()),
            "crates/{crate_dir} declares `{name}`, which is not one of {permitted:?}. {why}"
        );
    }
}

#[test]
fn the_engine_depends_on_nothing() {
    // The load-bearing direction, restated here so a reader of the test suite sees it.
    // rfq-core/build.rs fails the *build* on a violation; this fails the suite, and the
    // two are deliberately redundant because the property is invisible once broken.
    assert_dependencies_within(
        "core",
        &["proptest"],
        "The engine has zero dependencies (CLAUDE 36) and in particular never depends on \
         rfq-chain (SPEC §13.1, CLAUDE 8b). The only engine-to-custody path is a \
         SubmitIntent event through an adapter.",
    );
}

#[test]
fn custody_depends_on_shared_types_and_nothing_else() {
    assert_dependencies_within(
        "chain",
        &["rfq-core"],
        "Custody knows nothing of requests, quotes, legs, reservations or claims, and must \
         not learn about the runtime that wires it to the engine (SPEC §13.1).",
    );
}

#[test]
fn the_runtime_wires_the_two_systems_and_owns_neither() {
    assert_dependencies_within(
        "runtime",
        &["rfq-core", "rfq-chain", "proptest"],
        "The runtime holds both systems side by side; it is the only crate permitted to.",
    );
}

#[test]
fn nothing_depends_on_the_scenario_runner() {
    for crate_dir in ["core", "chain", "runtime"] {
        assert!(
            !declared_dependencies(crate_dir).iter().any(|name| name == "rfq-scenarios"),
            "crates/{crate_dir} depends on the scenario runner; scenarios are a leaf"
        );
    }
    assert_dependencies_within(
        "scenarios",
        &["rfq-core", "rfq-chain", "rfq-runtime"],
        "The scenario runner is a leaf binary.",
    );
}
