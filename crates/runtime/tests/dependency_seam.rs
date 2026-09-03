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
    // `criterion` is permitted **here and nowhere else**, and only as a dev-dependency: the
    // benchmarks measure the hand-offs out of the engine thread, which live in this crate.
    // `core` and `chain` keep their own lists, so this cannot leak into either.
    assert_dependencies_within(
        "runtime",
        &["rfq-core", "rfq-chain", "proptest", "criterion"],
        "The runtime holds both systems side by side; it is the only crate permitted to — \
         and it is the only crate permitted a benchmark harness, because the hot path it \
         measures is the one it owns.",
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


// ─────────────────── the separation check (SPEC §13.1, PLAN S3) ───────────────────

/// Every `.rs` file under a crate's `src`, as (path, contents).
fn sources(crate_dir: &str) -> Vec<(PathBuf, String)> {
    fn walk(dir: &std::path::Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                out.push((path, text));
            }
        }
    }
    let mut out = Vec::new();
    walk(&workspace_root().join("crates").join(crate_dir).join("src"), &mut out);
    assert!(!out.is_empty(), "no sources found for crates/{crate_dir}");
    out
}

/// The code, with comments and string literals blanked out.
///
/// Prose is where the seam is *explained*: `core`'s clock module says why custody holds its
/// own clock, and custody's module says what it is not allowed to know. Documentation naming
/// the other system is the design being written down, not the design being violated — so the
/// scan reads what compiles, not what is written about it.
fn code_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let mut in_string = false;
        let mut escaped = false;
        let mut chars = line.char_indices().peekable();
        while let Some((at, ch)) = chars.next() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '/' if chars.peek().is_some_and(|(_, next)| *next == '/') => {
                    let _ = at;
                    break;
                }
                _ => out.push(ch),
            }
        }
        out.push('\n');
    }
    out
}

/// Whether `needle` appears as a whole identifier, not as part of a longer one.
fn mentions(text: &str, needle: &str) -> bool {
    text.match_indices(needle).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at.saturating_add(needle.len())..].chars().next();
        let boundary = |ch: Option<char>| {
            ch.is_none_or(|ch| !ch.is_alphanumeric() && ch != '_')
        };
        boundary(before) && boundary(after)
    })
}

#[test]
fn no_path_in_the_engine_can_read_a_custody_balance() {
    // The compile-time half is rfq-core's build script: the engine declares no dependency,
    // so nothing in `chain` is nameable from it. This is the readable half — it says which
    // *concepts* are absent, and it fails on the first line of code that reaches for one.
    for (path, text) in sources("core") {
        let text = code_only(&text);
        for forbidden in ["Custody", "CustodyLedger", "Bundle", "BundleLeg", "rfq_chain"] {
            assert!(
                !mentions(&text, forbidden),
                "{} names `{forbidden}`. The engine holds an EscrowId and its own mirror, \
                 and nothing else crosses (SPEC §13.1).",
                path.display()
            );
        }
    }
}

#[test]
fn custody_has_never_heard_of_a_request_a_quote_or_a_claim() {
    // The other direction, and the one cargo does not constrain at all: `chain` depends on
    // `core`, so every engine type is nameable from it. What stops custody using them is
    // this.
    for (path, text) in sources("chain") {
        let text = code_only(&text);
        for forbidden in
            ["Engine", "Ledger", "Reservation", "ResIdx", "ResOwner", "QuoteIdx", "Request"]
        {
            assert!(
                !mentions(&text, forbidden),
                "{} names `{forbidden}`. Custody knows nothing of requests, quotes, legs, \
                 reservations or claims, and has never heard of a committed bucket \
                 (SPEC §13.1).",
                path.display()
            );
        }
    }
}

#[test]
fn the_harness_is_the_only_structure_that_holds_both() {
    // It may read both systems; no production path may. If anything else in the runtime
    // ever names both an Engine and a Custody, the seam has a second crossing point and the
    // conservation assertions are no longer the only thing that spans it.
    for (path, text) in sources("runtime") {
        let text = code_only(&text);
        let holds_engine = mentions(&text, "Engine");
        let holds_custody = mentions(&text, "Custody");
        if holds_engine && holds_custody {
            assert!(
                path.ends_with("harness.rs"),
                "{} holds both systems. Only the harness may (SPEC §13.1, CLAUDE 8c).",
                path.display()
            );
        }
    }
}


#[test]
fn no_production_path_in_the_runtime_can_reach_the_harness() {
    // The harness has **no production counterpart** (SPEC §13.1). In v2 its wiring is
    // replaced by real transport and its cross-system assertions become the reconciler — a
    // monitoring component that reports divergence rather than an oracle of truth that
    // prevents it. So the single-writer loop, the event ring, the gateway and the settlement
    // adapter must all be reachable without it, and none of them may name it.
    //
    // Which also settles where the test-only affordances live: `mirror_stale_for_test` is on
    // the harness, and nothing but a test or a scenario can hold a harness to call it.
    for (path, text) in sources("runtime") {
        if path.ends_with("harness.rs") || path.ends_with("lib.rs") {
            continue;
        }
        assert!(
            !mentions(&code_only(&text), "Harness"),
            "{} names the harness. It is a test-and-scenario structure and no production \
             path may reach it (SPEC §13.1, CLAUDE 8c).",
            path.display()
        );
    }
}

#[test]
fn the_harness_has_no_test_only_surface() {
    // It had one: a way to write a stale value into the engine's mirror, needed while the
    // custody-to-engine wire was a synchronous copy of a balance. With a real indexer —
    // cursor, confirmation depth, dedup — staleness is produced by not reading the log, which
    // is what lag is, so the affordance had nothing left to do.
    //
    // Asserted as an absence, because an affordance that exists will eventually be used, and
    // the harness is the one structure that can see both systems.
    for crate_dir in ["core", "chain", "runtime", "scenarios"] {
        for (path, text) in sources(crate_dir) {
            let code = code_only(&text);
            for marker in ["_for_test", "for_testing", "test_only"] {
                assert!(
                    !code.contains(marker),
                    "{} carries a test-only affordance ({marker})",
                    path.display()
                );
            }
        }
    }
}
