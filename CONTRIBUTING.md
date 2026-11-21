# Contributing to KeyDock

Open an issue for reproducible problems, especially changes that affect keyboard detection or Windows input state. Include your Windows version, architecture, the keyboard connection type, and a minimal configuration with personal paths and identifiers removed.

Run these checks before opening a pull request.

```powershell
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

Windows CI exercises native process, mutex, and Job Object behavior with test-owned processes and names. The ignored live language-order test changes Windows input settings and belongs in a disposable console account. The ignored old-job-reuse diagnostic is for investigating host-specific behavior.

Use the [release guide](docs/releasing.md) for packaging and publication.
