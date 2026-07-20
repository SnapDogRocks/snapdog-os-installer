# Contributing

## Commit messages

All commits and pull-request titles must follow
[Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):

```text
<type>[optional scope][!]: <description>
```

Accepted types are `build`, `chore`, `ci`, `docs`, `feat`, `fix`, `perf`,
`refactor`, `revert`, `style`, and `test`.

Examples:

```text
feat(catalog): add development channel
fix(macos): preserve dock icon transparency
refactor(worker)!: remove the legacy protocol
```

CI validates every commit in a pull request as well as its title. The `main`
branch ruleset requires the complete CI matrix and allows squash merges only,
so the validated pull-request title becomes the commit subject on `main`.
Commits preceding the introduction of this policy are grandfathered.
