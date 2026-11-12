## What and why

<!-- What this changes and why. Link the issue: Fixes #123 -->

## How you tested it

<!-- Commands you ran, what you clicked in the dashboard, and your setup
(k3s version, number of servers). -->

## Checklist

- [ ] The title follows [Conventional Commits](https://www.conventionalcommits.org) (`feat(api): ...`)
- [ ] The [checks](https://github.com/upsetdev/pyo/blob/main/CONTRIBUTING.md#run-the-checks) pass: `cargo test --locked --workspace`, `bun run typecheck`, `bun run build`, `bun run test`
- [ ] `waf/build.sh` passes, if `waf/` changed
- [ ] Database changes are a new migration (see [Change the database](https://github.com/upsetdev/pyo/blob/main/RELEASING.md#change-the-database))
- [ ] Docs updated in [pyo-docs](https://github.com/upsetdev/pyo-docs), if behaviour or options changed
- [ ] Screenshots in light and dark mode, for dashboard changes
