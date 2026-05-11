# Templates

Page routes (`kind: "page"`, `method: "GET"`) render HTML by way of
[Askama](https://crates.io/crates/askama) templates. A template file lives
under `<project>/templates/` and is referenced from a route by its relative
path:

```json
{ "path": "/", "method": "GET", "kind": "page", "template": "home.html" }
```

At build time, `rublocks` mirrors the entire `templates/` tree into
`<project>/dist/templates/` so Askama's compile-time path resolver finds the
files. Editing a template under `<project>/templates/` triggers the dev
watcher and rebuilds the dist.

## Generated context struct

For each `kind: page` GET route, codegen emits a typed context struct in a
dedicated module:

```rust
pub mod ctx_home {
    #[derive(askama::Template, Default)]
    #[template(path = "home.html")]
    pub struct PageContext {
        pub page_title: String,
        pub current_year: String,
        pub posts: Vec<crate::models::Post>,
    }
}
```

Field set comes from the union of:

1. `layout.requires` (typed declarations — currently always `string`).
2. `layout.view` (the layout's own view bindings).
3. `route.view` (route-level bindings; route values win on conflict).

Field types are inferred from `route.process` and `layout.process`:

- `"$<name>"` where `<name>` matches a `db.find_many` block on table `T` →
  `Vec<crate::models::<T-as-model>>`.
- `"$<name>"` where `<name>` matches a `db.find_one` block on table `T` →
  `crate::models::<T-as-model>`.
- `"$<name>.<field>"` and unrecognized references fall back to `String`.
- Literal values are kept as `String`.

## Default values

Until slice 5 wires real process execution, every page context is built with
`Default::default()` for every field. Literal `route.view` values
(e.g. `"page_title": "Recent posts"`) are baked into the handler and
override the default. Everything else renders as the type's default — empty
strings, empty `Vec`s, nil UUIDs, epoch timestamps.

Nullable model fields (`"nullable": true`) generate as
`crate::_rb_util::NullDisplay<T>` rather than `Option<T>`. The wrapper is
transparent for serde and sqlx but renders the inner `Display` (or empty)
under Askama, so templates can reference nullable columns directly without
fighting `Option<T>: !Display`.

## Layout inheritance

When a route declares `"layout": "<name>"`, Askama's `{% extends %}`
takes over. The user's page template starts with:

```jinja
{% extends "layout.html" %}
{% block content %}
  ...
{% endblock %}
```

The layout's `template` (from `layouts/<name>.json`) and the page's
`template` must both live under `templates/` — Askama resolves the
`{% extends %}` path against the same templates directory.

## Dev-mode livereload

Every rendered page passes through a small `maybe_inject_dev_snippet`
helper. When `RUBLOCKS_DEV=1` is set, the helper inserts a
`<script src="/__rublocks/livereload.js"></script>` tag before `</body>`
so the browser reconnects after every rebuild. The non-dev path is a single
env-var lookup per request.

## Errors

- Missing template file → cargo build error surfaced via the dev overlay
  (Askama's `derive(Template)` macro panics at compile time with the
  expected path).
- Unknown layout name (`route.layout` doesn't match any
  `layouts/*.json`) → manifest validation error pointing at the offending
  route file.
- Field-on-model mismatch (template uses `{{ post.title }}` but `Post`
  has no `title`) → cargo build error via the Askama derive.

## Limits (slice 3)

- Process blocks are parsed but not executed; every reference resolves to
  the type's default at runtime.
- `input.{path,query,body}` is accepted by the parser but never consumed
  by the handler.
- `view` values must be JSON strings. Numbers, booleans, and nested
  objects are not accepted yet.
