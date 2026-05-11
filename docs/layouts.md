# Layouts

A layout is a named wrapper template that any number of pages can opt into
via `route.layout: "<name>"`. Layouts live under `<project>/layouts/`,
one JSON file per layout.

## Schema

<!-- rb:layout -->
```json
{
  "name": "main",
  "template": "layout.html",

  "requires": {
    "page_title": { "type": "string" }
  },

  "process": [],
  "view": {
    "current_year": "$year"
  }
}
```

- `name` *(required)* — identifier referenced from
  `route.layout`. Must be unique across all `layouts/*.json`.
- `template` *(required)* — HTML file under `templates/`. Askama
  resolves `{% extends "<this>" %}` against the same directory.
- `requires` — optional declaration of variables the layout expects the
  calling page to supply. Each entry becomes a field on the generated
  page context struct. `type: "string"` is the only type allowed today.
- `process` — same shape as `route.process`: an ordered list of
  [blocks](blocks/README.md) dispatched against the same registry. The
  layout's blocks run before the route's blocks, and their bindings are
  available to both the layout's `view` and the page template.
- `view` — view bindings exposed by the layout. Merged into the page
  context fields so Askama inheritance can read them.

## Resolution

At manifest load time, every `route.layout` is cross-checked against the
loaded layout set. Unknown names produce a `ManifestError` whose `file`
points at the offending route — the user-actionable place to edit.

## Templates

Both the layout's `template` and the page's `template` are subject to
the rules in [templates.md](templates.md). The user-side `{% extends %}`
declaration in the page template is what wires the inheritance — codegen
does not preprocess templates.

