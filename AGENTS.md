# AGENTS.md

How AI agents work in this repo. See [`README.md`](README.md).

## Git: stakkr

This repository's source of truth is **stakkr** (stakkr spec 003): `origin` is
`https://stakkr.lab.harbor.openairlab.cloud/poltronaut/bevy.git`. GitHub (the `github`
remote) is a downstream mirror of `main` and tags.

- Push branches to `origin`. Pull requests: `sk pr create --fill`, `sk pr checks --watch`,
  `sk pr merge` (`sk` is `stakkr-cli` with gh's verbs). CI logs: `sk run view --log`. People
  review and merge in the skæl app.
- Never use `gh pr` here, and never push to the `github` remote: a change on GitHub halts that
  ref's mirror. Only if stakkr is down, push to `github` and tell the operator, who clears the
  halt before the next push to stakkr.
- The desktop app's PR bar, Auto-fix and CI monitor speak GitHub only and don't see this
  repository's pull requests. A branch pushed to stakkr and absent from GitHub is expected.
