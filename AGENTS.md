# Repository Guidelines

## Purpose and boundaries

`tinyskills` owns portable handling of agentskills.io-style bundles: metadata,
document parsing, filesystem discovery, collision handling, safe resource
reads, and compile-time bundle materialization.

Embedding products own directory layouts, workspace trust, installation
policy, execution, approvals, RPC/controller schemas, event buses, and UI.
Keep product names, product environment variables, and product-specific paths
out of this crate; accept roots and policy inputs at the API boundary instead.

## Structure

```text
Cargo.toml
crates/tinyskills/
├── Cargo.toml
├── src/
│   ├── lib.rs          # public exports and crate overview
│   ├── model.rs        # metadata, scopes, constants
│   ├── document.rs     # Markdown/frontmatter parsing and inventory
│   ├── discovery.rs    # deterministic scanning and collisions
│   ├── resource.rs     # safe lookup and resource reads
│   └── bundle.rs       # compile-time bundle materialization
└── tests/public_api.rs
```

Public exports are centralized in `lib.rs`. Prefer focused modules named for
their responsibility; do not introduce generic `utils` or `helpers` modules.

## Build and test

Run all commands from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test --all-features
```

Tests must be deterministic and avoid network access, wall-clock assumptions,
and shared process state. Integration tests exercise the public API. Add a
regression test for each bug and cover failure paths for security-sensitive
filesystem behavior.

## Rust style

- Use Rust 2024 idioms and standard `rustfmt` output.
- Keep the public surface minimal and document every public item.
- Return typed results from fallible public APIs and document `# Errors`.
- Do not use `unwrap`, `expect`, `panic`, `todo`, or `unimplemented` in library
  paths.
- Do not weaken workspace lints or add broad `allow` attributes.
- Reject symlinks before traversal and canonicalize before containment checks.
- Keep reads size-bounded and preserve strict UTF-8 behavior.

## Dependencies

Prefer the standard library and existing dependencies. Declare shared
dependencies once in the root workspace manifest, enable only required
features, and document any substantial new dependency. Keep `Cargo.lock`
committed.

## Documentation

Update `README.md`, rustdoc, and tests with behavior or public API changes.
Write for an embedding application that has never seen the original host.
Examples must compile and must not assume an OpenHuman directory layout.

## Git workflow

- Never work directly on `main`; use a feature worktree and branch.
- Do not rewrite published history or bypass hooks.
- Keep commits focused and use concise imperative subjects.
- Open pull requests ready for review unless they are genuinely incomplete.
- Report the exact validation commands run and any known failures.

## Agent working agreement

Read surrounding code before editing, stay within the requested scope, and
verify claims with fresh command output. Do not leave placeholders, weaken
guardrails, inspect secrets, or silently skip failing tests. Ask only when an
irreversible decision or genuine product-policy fork blocks progress.
