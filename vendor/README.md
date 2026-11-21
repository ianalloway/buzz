# Cargo dependency patches

Both the root workspace and the standalone desktop workspace use these sources. Remove the corresponding Cargo patches from both manifests when the upstream constraints support the same versions and backends.

## mesh-llm-log-store

Source is the `mesh/crates/mesh-llm-log-store` directory from [Mesh LLM v0.79.0](https://github.com/Mesh-LLM/mesh-llm/tree/e8c9e7fba8bf88b3fa65a4187862d1147dce372c/mesh/crates/mesh-llm-log-store). Its Rust sources, SQL migrations and tests match that revision. The manifest spells out the inherited workspace settings, points `mesh-llm-events` to the same tag, and updates `rusqlite` to 0.41.1. This resolves `libsqlite3-sys` 0.39.1 with its patched bundled SQLite. The desktop application uses the same SQLite version. [RustSec advisory](https://rustsec.org/advisories/RUSTSEC-2026-0459.html)

## keyring

Source is the [published keyring 4.2.0 crate](https://crates.io/crates/keyring/4.2.0) at [upstream revision d18b5c89](https://github.com/open-source-cooperative/keyring-rs/tree/d18b5c89a0a4e9192eebac82a508a1607378ea15). The published archive has SHA-256 `2270074a3d26bcac93c1dc5d2845eb4c089e8d761ccf6e0ea266a16004640627`. Its licenses are included.

The `v1` facade selects the crate’s existing libdbus Secret Service backend on supported Unix desktops. This preserves the system Secret Service collection, service/user credential lookup attributes, and encrypted transport while removing its dependency on the retired standalone zbus types. The macOS and Windows native backends and all public APIs retain their upstream behavior. The package manifest and the Unix store constructor in `src/v1.rs` are the only functional changes.
