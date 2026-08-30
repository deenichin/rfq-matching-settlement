//! Compile-time enforcement of the engine/custody seam.
//!
//! SPEC §13.1 and CLAUDE 8b: `core` is the engine and must never depend on `chain`.
//! Cargo enforces the *use* of an undeclared crate, but nothing stops the dependency
//! from being declared — and once it is, every later claim about the v1/v2 seam becomes
//! quietly untrue with no visible symptom. So the manifest itself is the thing checked.
//!
//! The check is deliberately wider than "not chain": `core` has *zero* dependencies
//! (CLAUDE 36), so any normal or build dependency at all is a failure, and the only
//! admissible dev-dependency is `proptest`. A rule stated as a whitelist cannot be
//! satisfied by renaming the thing it forbids.
//!
//! A build script is the mechanism because it fails `cargo build`, `cargo test` and
//! `cargo clippy` alike, before a single line of the crate is compiled.

// Cargo directives are emitted on stdout; this is the mechanism, not I/O by the engine.
#![allow(clippy::print_stdout)]

/// The sole dependency any target of this crate may declare, and only for tests.
const ALLOWED_DEV_DEPENDENCIES: [&str; 1] = ["proptest"];

fn main() {
    println!("cargo::rerun-if-changed=Cargo.toml");
    println!("cargo::rerun-if-changed=build.rs");

    let manifest = match std::fs::read_to_string("Cargo.toml") {
        Ok(text) => text,
        Err(err) => panic!("rfq-core: cannot read its own manifest to verify the seam: {err}"),
    };

    for (table, name) in declared_dependencies(&manifest) {
        match table {
            Table::Dev if ALLOWED_DEV_DEPENDENCIES.contains(&name.as_str()) => {}
            Table::Dev => panic!(
                "rfq-core declares dev-dependency `{name}`. CLAUDE 36: proptest is the only \
                 dev-dependency permitted. If that is the custody crate, this is the SPEC \
                 §13.1 seam violation the check exists for, and a test is not an exemption: \
                 cargo permits a dev-dependency cycle, so this is the one direction it does \
                 not refuse on its own."
            ),
            Table::Normal | Table::Build => panic!(
                "rfq-core declares dependency `{name}`. The engine has zero dependencies \
                 (CLAUDE 36) and in particular never depends on rfq-chain (SPEC §13.1, \
                 CLAUDE 8b): the only engine-to-custody path is a SubmitIntent event \
                 through an adapter."
            ),
        }
    }
}

/// Which dependency table an entry was declared in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Table {
    Normal,
    Build,
    Dev,
}

/// Every dependency name declared by the manifest, with the table it came from.
///
/// A hand-rolled scan rather than a TOML parser, because pulling one in to check that
/// this crate has no dependencies would be self-defeating. It covers the four forms
/// cargo manifests use: `[dependencies]` tables, `[dependencies.name]` sub-tables,
/// `name = "1"` and `name = { workspace = true }` / `name.workspace = true` entries,
/// and the `[target.'cfg(..)'.dependencies]` variants of each.
fn declared_dependencies(manifest: &str) -> Vec<(Table, String)> {
    let mut found = Vec::new();
    let mut table = None;

    for raw in manifest.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }

        if let Some(header) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let header = header.trim();
            // `[target.'cfg(unix)'.dependencies]` and friends: only the tail decides.
            let tail = header.rsplit('.').next().unwrap_or(header);
            table = match tail {
                "dependencies" => Some((Table::Normal, header.to_owned())),
                "build-dependencies" => Some((Table::Build, header.to_owned())),
                "dev-dependencies" => Some((Table::Dev, header.to_owned())),
                _ => {
                    // `[dependencies.rfq-chain]` names the dependency in the header.
                    if let Some((kind, name)) = sub_table(header) {
                        found.push((kind, name));
                    }
                    None
                }
            };
            continue;
        }

        if let Some((kind, _)) = table
            && let Some((key, _)) = line.split_once('=')
        {
            // `name.workspace = true` and `name.version = "1"` both name `name`.
            let name = key.trim().trim_matches('"').split('.').next().unwrap_or("").trim();
            if !name.is_empty() {
                found.push((kind, name.to_owned()));
            }
        }
    }

    found
}

/// A `[dependencies.name]`-shaped header, decomposed into its table and dependency name.
fn sub_table(header: &str) -> Option<(Table, String)> {
    let (prefix, name) = header.rsplit_once('.')?;
    let kind = match prefix.rsplit('.').next().unwrap_or(prefix) {
        "dependencies" => Table::Normal,
        "build-dependencies" => Table::Build,
        "dev-dependencies" => Table::Dev,
        _ => return None,
    };
    Some((kind, name.trim().trim_matches('"').to_owned()))
}

/// Everything before the first `#` outside a quoted string.
fn strip_comment(line: &str) -> &str {
    let mut in_quotes = false;
    for (i, ch) in line.char_indices() {
        match ch {
            '"' | '\'' => in_quotes = !in_quotes,
            '#' if !in_quotes => return line.get(..i).unwrap_or(""),
            _ => {}
        }
    }
    line
}
