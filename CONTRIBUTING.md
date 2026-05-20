# Contributing to hintkit

Thanks for your interest. A few things to know before you spend time on a contribution.

## Status: pre-alpha, solo-maintained

The project is in early scaffolding. Until v0.1 ships, the maintainer is intentionally working alone to keep architectural decisions coherent. That means:

- **Feature PRs will likely be closed without merge until v0.1.** This is not about your code — it's about keeping the design surface small while it's still being shaped.
- **Bug reports and discussion are welcome.** If you spot something that contradicts the README, file an issue.
- **Completion-spec requests are tracked but not implemented on demand.** The v0.1 set is fixed at ~30 commands.

After v0.1 ships, this policy opens up. Watch [CHANGELOG.md](CHANGELOG.md) for the v0.1 release announcement.

## Project anti-goals

Before proposing a change, please understand what `hintkit` is **not** trying to be. The following will not be accepted, ever, regardless of implementation quality:

- Telemetry, analytics, or any form of phone-home
- Account systems, login, or cloud sync
- Required network access for the core suggestion experience
- A GUI, system tray, or desktop app component
- AI features in the core binary (a separate optional plugin may exist later)
- A subscription model or any commercial layer

These are listed in the README for the same reason — they are load-bearing identity decisions, not "not yet" deferrals.

## If you do want to contribute

1. **Open an issue first** describing what you want to change and why. Don't write code before there's agreement on the approach.
2. **Keep PRs small.** One change per PR.
3. **Run the formatter and linter** before pushing:
   ```
   cargo fmt
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
4. **By submitting a contribution, you agree to dual-license it under MIT and Apache-2.0**, matching the project license. No CLA is required.

## Reporting security issues

See [SECURITY.md](SECURITY.md). Do not file public issues for security vulnerabilities.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By participating, you agree to abide by its terms.
