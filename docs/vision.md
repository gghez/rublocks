# Vision

## What rublocks is

rublocks is a declarative programming language for building web applications. Source files are JSON; the compiler emits a complete Rust project targeting [Axum](https://github.com/tokio-rs/axum).

## Who it is for

Coding agents (LLMs). The premise: when intent is encoded as well-shaped JSON blocks, an agent's job collapses to "fill in the right slots" rather than reasoning about Rust syntax, lifetimes, or framework idioms.

A human can author rublocks too, but the language's affordances (verbose, declarative, redundancy-tolerant) are tuned for agent ergonomics, not human terseness.

## What rublocks is not

- Not a templating system. The output is a real Rust project that builds with `cargo build`.
- Not a no-code platform. The user (or agent) declares structure; the compiler chooses the implementation.
- Not a runtime. There is no rublocks interpreter at runtime — only generated Rust.

## Scope

First milestone: web applications. Future milestones may extend to other application archetypes (CLI tools, background workers, ETL pipelines), but the initial design is shaped by HTTP-server concerns.

See [decisions.md](decisions.md) for the rationale behind each major choice.
