# Architecture

## Components

The rublocks compiler is a single Rust binary (`rublocks`) with the following modules:

| Module | File | Role |
|--------|------|------|
| `manifest` | `src/manifest.rs` | Parse and validate `main.json` into a typed `Manifest`. |
| `codegen` | `src/codegen.rs` | Generate `Cargo.toml` and `src/main.rs` for the target Rust project. |
| `dev` | `src/dev.rs` | Dev-mode supervisor: watch JSON files, rebuild + restart on change. |
| `main` | `src/main.rs` | CLI entry point (clap), routes subcommands to the modules above. |

## Data flow

```
                  ┌────────────┐
   main.json ───▶ │  manifest  │ ───▶ Manifest struct
                  └────────────┘
                                            │
                                            ▼
                                     ┌────────────┐
                                     │  codegen   │ ───▶ dist/Cargo.toml
                                     └────────────┘     dist/src/main.rs
                                            │
                                            ▼
                                     ┌────────────┐
                                     │  cargo     │ ───▶ dist/target/debug/<app>
                                     │  build     │
                                     └────────────┘
                                            │
                                            ▼ (dev mode)
                                     ┌────────────┐
                                     │  dev       │ ───▶ child process + file watcher
                                     │ supervisor │
                                     └────────────┘
```

## Code generation strategy

Rust code is built as a `proc_macro2::TokenStream` via `quote::quote!`, then parsed with `syn::parse2` and pretty-printed with `prettyplease::unparse`. This guarantees the emitted code is syntactically valid and well-formatted, and avoids the fragility of string templates.

`Cargo.toml` is the one exception: it is emitted as a string because TOML is not a Rust target.

See [decisions.md](decisions.md#codegen-quote--prettyplease) for the rationale.

## Generated project layout

```
<project>/
├── main.json
└── dist/                     # generated, gitignored
    ├── Cargo.toml
    ├── Cargo.lock
    ├── src/
    │   └── main.rs
    └── target/               # cargo build artifacts (preserved across regenerations)
```

The `dist/target/` directory is intentionally **not** wiped between regenerations so cargo can rebuild incrementally.

## Conditional dependencies

Each declared service in `main.json` adds dependencies to the generated `Cargo.toml`:

| Service | Crate(s) added |
|---------|----------------|
| `postgres` | `sqlx` (with `runtime-tokio`, `tls-rustls`, `postgres` features) |
| `redis` | `deadpool-redis` (with `rt_tokio_1`) |

Always present in the generated project: `axum`, `tokio`, `anyhow`, `futures-util`.
