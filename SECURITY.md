# Security Policy

## Reporting a vulnerability

If you believe you have found a security vulnerability in `hintkit`, please **do not** open a public GitHub issue. Instead, report it privately by one of the following channels:

- **GitHub Security Advisories** (preferred): open a draft advisory at <https://github.com/ahsanjaved001/hintkit/security/advisories/new>
- **Email**: ahsanjvd001@gmail.com — feel free to encrypt with the maintainer's public key on request

Please include:

- A description of the issue and the impact you believe it has
- Steps to reproduce, including the affected version, operating system, and shell
- Any proof-of-concept code or commands (please keep these minimal)

You can expect:

- An acknowledgment within **7 days**
- An initial assessment within **14 days**
- A coordinated fix and disclosure timeline agreed with you before any public discussion

The project is solo-maintained on a best-effort basis. There is no formal SLA, but security reports are prioritized over feature work.

## Supported versions

`hintkit` is pre-alpha. There is no released version yet, so there is no supported-version matrix. Once v0.1 ships, security fixes will be backported to the most recent minor version only.

## Scope

Things that are in scope for security reports:

- The released `hintkit` binary
- Shell integration scripts (`hintkit.zsh`, `hintkit.bash`)
- The install script (`install.sh`)
- The build-time spec ingestion pipeline, where it might allow malicious specs to compromise the build
- GitHub Actions workflows that have repository write access

Things that are out of scope:

- Theoretical attacks against pre-release code on the `main` branch where no release has been cut
- Vulnerabilities in upstream `withfig/autocomplete` specs themselves (report those to that project)
- Denial-of-service via crafted input that affects only the local user's own session
- Social engineering, physical access, or anything that requires the attacker to already have full shell access on the victim's machine

## Safe harbor

We will not pursue legal action against researchers who:

- Make a good-faith effort to comply with this policy
- Do not exploit vulnerabilities beyond the minimum needed to demonstrate them
- Do not access, modify, or destroy data belonging to other users
- Give us a reasonable window to remediate before public disclosure (typically 90 days)
