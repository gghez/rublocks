# Vision

## What rublocks is

rublocks is a declarative programming language for building web applications. Source files are JSON; the compiler emits a complete Rust project targeting [Axum](https://github.com/tokio-rs/axum).

The name is its design statement: **rublocks = rust blocks**. Every route's behaviour is a *composition of [blocks](blocks/README.md)* — small declarative steps with a standardised input/output contract. New capabilities ship as new block kinds, each with its own JSON surface and doc page under `docs/blocks/`.

## Who it is for

Coding agents (LLMs). The premise: when intent is encoded as well-shaped JSON blocks, an agent's job collapses to "fill in the right slots" rather than reasoning about Rust syntax, lifetimes, or framework idioms.

A human can author rublocks too, but the language's affordances (verbose, declarative, redundancy-tolerant) are tuned for agent ergonomics, not human terseness.

## Why not just generate Rust directly?

The first review reflex on any code-gen project is "why a middle layer? have the agent emit Rust." Three reasons rublocks holds the JSON-shaped middle layer:

- **Typed slots beat free-form code.** Filling well-shaped JSON fields is a task LLMs are demonstrably reliable at. Reasoning about Rust trait gymnastics, lifetimes, Axum extractor signatures, and sqlx macros is a task they are not. The block catalogue defines the slots; the agent's job collapses to picking the right block and filling each one.
- **Canonical JSON ⇒ idempotent output.** Each declarative form has exactly one spelling (see [decisions.md](decisions.md)), so the same intent always maps to the same generated Rust. An agent can re-emit a route after a tiny edit without churning unrelated handlers — no diff noise, no surprise regressions, reviewable PRs.
- **The escape hatch is `dist/` itself.** The output is idiomatic, readable, `cargo build`-able Rust. If rublocks ever stops being useful for a project, the generated crate stays valid — copy `dist/` out, drop the JSON, keep going. No runtime, no interpreter, no lock-in.

## What rublocks is not

- Not a templating system. The output is a real Rust project that builds with `cargo build`.
- Not a no-code platform. The user (or agent) declares structure; the compiler chooses the implementation.
- Not a runtime. There is no rublocks interpreter at runtime — only generated Rust.

## Scope

First milestone: web applications. Future milestones may extend to other application archetypes (CLI tools, background workers, ETL pipelines), but the initial design is shaped by HTTP-server concerns.

See [decisions.md](decisions.md) for the rationale behind each major choice.
