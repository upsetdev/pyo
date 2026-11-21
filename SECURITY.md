# Security

## Report a vulnerability

Please don't report it in a public issue. Tell us privately instead:

- on GitHub: [report a vulnerability](https://github.com/upsetdev/pyo/security/advisories/new)
- or by email to support@pyo.dev, with "Security" in the subject

Say what's affected, how to reproduce it and which version you run (it's
under **Settings › Updates**). We'll confirm we got it, keep you posted on
the fix and credit you in the advisory if you'd like.

This covers Pyo, its CLI, its install scripts and its images at
`ghcr.io/upsetdev/pyo`. Pyo Sandbox's templates live in
[upsetdev/pyo-sandbox-images](https://github.com/upsetdev/pyo-sandbox-images),
and you can report those here too.

## Supported versions

| Version | Security fixes |
|---|---|
| The latest release | Yes |
| Older releases | No. Update to the latest |

A fix comes as a patch release, like 0.5.1 after 0.5.0. A self-hosted Pyo
shows it in the update notice and under **Settings › Updates**, and applies
it from there or by itself when automatic updates are on. Pyo Cloud is
updated for you.
