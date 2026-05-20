# hintkit

> A lightweight, local, no-account terminal autocomplete. Fig-style inline suggestions without the bloat. A kit of hints, for your shell.

`hintkit` is a CLI tool that shows IDE-style inline suggestions in your terminal as you type — matching against a database of completion specs for common commands (git, npm, docker, kubectl, and so on).

It is a successor in spirit to Fig, deliberately scoped down:

- **Lightweight** — single static binary, no daemon, no GUI, no background services
- **Local-only** — no accounts, no cloud, no sync, no telemetry, ever
- **Offline-capable** — works on a plane, in a container, over SSH, in air-gapped environments
- **Fast** — sub-30ms suggestion latency targeted

It is **not** Fig, Amazon Q, or Kiro. Those tools include auth, cloud sync, AI chat, and desktop apps. `hintkit` deliberately does not.

## Status

**Pre-alpha.** Not yet usable. The project is in early scaffolding. There is no installable release. Star the repo if you want to be notified when v0.1 ships.

## What's planned for v0.1

- Linux (x86_64, arm64) and macOS (arm64, x86_64)
- zsh and bash (≥ 4.0)
- Suggestions for the ~30 most common CLI tools, ingested from the MIT-licensed `withfig/autocomplete` specs
- Inline ANSI popup, arrow keys to navigate, Tab to accept, Esc to dismiss
- `curl | sh` install script, GitHub Releases

## What's explicitly *not* planned

These are not "not yet" — they are deliberate anti-goals:

- Telemetry, analytics, or phone-home of any kind
- Required accounts, login, or cloud sync
- AI chat or "natural language to command" in the core (a separate optional plugin may exist later)
- A GUI, desktop app, or system tray
- Subscriptions, paid tiers, or any commercial layer

If you want those features, Fig / Amazon Q / Kiro / Warp exist and serve them well. `hintkit` is for people who want the inline-suggestion experience without them.

## Building from source

Build instructions will be added when there is code to build.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.

## Attribution

`hintkit` reuses completion specifications from [`withfig/autocomplete`](https://github.com/withfig/autocomplete), distributed under the MIT License. See [NOTICE](NOTICE) for details.
