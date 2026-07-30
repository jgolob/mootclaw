//! Guards the purity of `zeroclaw-ledger`.
//!
//! The correctness-critical logic of the shared ledger — projection, the round
//! state machine, currency checks, admission — is pure by design: values in,
//! values out, no I/O and no clock reads. That is what makes it exhaustively
//! testable without a database, a container, or a model provider.
//!
//! It is easy to lose by accident. One convenience import of an async runtime
//! or a database client turns a pure decision function into something that can
//! only be tested against live infrastructure. This test makes that regression
//! fail in CI instead of in review.

use std::collections::BTreeSet;

/// Dependencies that would compromise the crate's purity.
const FORBIDDEN: &[&str] = &[
    "tokio",
    "sqlx",
    "axum",
    "reqwest",
    "hyper",
    "async-trait",
    "rusqlite",
    "tokio-postgres",
];

#[test]
fn crate_declares_no_runtime_database_or_http_dependencies() {
    let manifest_path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let manifest = std::fs::read_to_string(manifest_path).expect("read own Cargo.toml");
    let manifest: toml::Value = toml::from_str(&manifest).expect("parse own Cargo.toml");

    // `[dev-dependencies]` is deliberately not checked: test-only tooling does
    // not affect what the shipped crate can do.
    let declared: BTreeSet<String> = manifest
        .get("dependencies")
        .and_then(toml::Value::as_table)
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default();

    let violations: Vec<&str> = FORBIDDEN
        .iter()
        .copied()
        .filter(|name| declared.contains(*name))
        .collect();

    assert!(
        violations.is_empty(),
        "zeroclaw-ledger must stay pure — remove {violations:?} and move that work to a crate above \
         this one. Declared dependencies: {declared:?}"
    );
}
