# rublocks

Declarative JSON language that compiles to Rust/Axum web applications. Designed to be authored primarily by coding agents — declare your app in JSON, get a clean Rust project.

**Status: pre-alpha. Not usable yet.**

## Concept

You write a `main.json` declaring your app and its services:

```json
{
  "name": "myapp",
  "services": {
    "postgres": { "url": "env:DATABASE_URL" },
    "redis":    { "url": "env:REDIS_URL" }
  }
}
```

Then:

```bash
rublocks build   # generates a Rust/Axum project under ./dist
rublocks run     # build + cargo run
```

## Roadmap

- **v0** — CLI skeleton, `main.json` parsing, Cargo project generation with optional postgres/redis wiring, `/health` endpoint.
- **v1** — Route declarations (HTTP methods, paths, handlers).
- **v2** — Model and migration declarations.
- **v3** — Background jobs.

## License

MIT
