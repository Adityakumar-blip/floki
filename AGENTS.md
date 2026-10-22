# Floki contributor instructions

Floki is a native API client written in Rust with GPUI. Colors and sizes live in `src/theme.rs`; use those tokens rather than hard-coded values.

Keep the workbench layout, navigation, control placement, request/response flow, density, focus states and keyboard behavior consistent across features. Use only original or properly licensed code and assets.

Before committing code changes, run `cargo build --locked` and `cargo test --locked` (`dev.cmd check --locked` / `dev.cmd test --locked` on Windows). Keep compiled output and local workspace data out of Git.
