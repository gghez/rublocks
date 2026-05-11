# CLI reference

The canonical reference for every command and flag is the binary itself:

```
rublocks help            # top-level overview
rublocks help <command>  # full long-form help for a command
```

Each subcommand carries its own `long_about` and an `Examples` section — `clap` renders them with no extra step. Keeping the reference in the binary is intentional: it cannot drift from the code, and it works offline.

## Commands at a glance

| Command | Status | What it does |
|---------|--------|--------------|
| `rublocks new <name>` | not implemented | Will scaffold a fresh project directory. |
| `rublocks build [path]` | implemented | One-shot codegen pass: parses the project, generates migrations from model diffs, rewrites `<path>/dist/`, and refreshes the per-agent integration files. Does NOT invoke `cargo build`. |
| `rublocks run [path]` | not implemented | Will build then run the generated binary without watching. |
| `rublocks dev [path]` | implemented | Full iteration loop: build + `cargo build` + run + watch + livereload, with ephemeral postgres/redis when service URLs use `env:VAR`. |

`[path]` defaults to the current directory and is canonicalized before use.

## Generated-binary migration verbs

When a project declares a database and migrations exist, the generated binary exposes:

```
./<app>                # serve (default)
./<app> migrate        # apply pending migrations and exit 0
./<app> migrate --list # list every migration with state
```

See [migrations.md](migrations.md) for the full migration story and [dev-mode.md](dev-mode.md) for the dev-loop protocol.
