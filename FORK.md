# Our Bevy fork

The workspace takes Bevy from our fork, [`poltronaut/bevy`](https://github.com/poltronaut/bevy),
so we can carry changes Bevy doesn't have yet (the first: retained shadow
maps, matter-engine spec 065) while still taking every upstream update. The
decision and what lost are in spec 065 D5; this page is the procedure.
`tools/bevy_fork.sh` (in matter-engine) runs every step; the fork's copy of
this page is its `FORK.md`.

## Branches

| Branch | What it holds | Who writes it |
|---|---|---|
| `main` | the upstream release we build on (today `v0.19.1`) plus our patches | merges only: `patch/*` and `sync/*` PRs |
| `upstream-main` | `bevyengine/bevy` `main`, fast-forward only | `tools/bevy_fork.sh mirror --push` |
| `patch/<topic>` | one change of ours, cut from the release `main` is on | whoever writes the patch |
| `pr/<topic>` | the same change on `upstream-main`, offered upstream | when it could go upstream |
| `sync/<tag>` | an upstream release merged into `main` | `tools/bevy_fork.sh sync <tag>` |

```
bevyengine/bevy main ──► upstream-main                 (fast-forward mirror)
bevyengine/bevy v0.19.1 ─┐
                         ├─► main                       (release + our patches, merges only)
patch/<topic> ───────────┘
patch/<topic> ───────────► pr/<topic> on upstream-main   (offered upstream)
```

Why `main` is a release and not upstream `main`: upstream `main` is the next
version in development (`0.20.0-dev` on 2026-10-08), while avian3d, bevy-tnua
and the other Bevy crates we use pin the release we're on. `[patch.crates-io]`
only replaces a crate with a semver-compatible one.

Why merges and never a rebase: every `Cargo.lock` in matter-engine's history
pins a revision of `main`. A rebase would orphan those revisions and old
commits would stop building.

## Day to day

```bash
tools/bevy_fork.sh status
```

Shows the release `main` is on, our patches, upstream releases not merged
yet, a newer minor version if there is one, and how far `upstream-main`
lags. Mirror upstream whenever you like (nothing builds from it):

```bash
tools/bevy_fork.sh mirror --push
```

## Taking an upstream patch release (`v0.19.2`)

```bash
tools/bevy_fork.sh sync v0.19.2
git -C ../bevy push origin sync/v0.19.2
gh pr create --repo poltronaut/bevy --base main --head sync/v0.19.2 --title 'Merge upstream v0.19.2'
```

Merge the PR (a merge commit, not a squash or rebase). Then, in matter-engine,
on a `fix-` or `tool-` branch: `cargo update -p bevy` moves every Bevy crate to
the new revision together. Run level 4 tests (AGENTS.md §5) and the lab
segments that touch our patches.

## Upgrading Bevy (`v0.20.0`)

An upgrade is a workspace change (AGENTS.md §1: one Bevy version, upgraded
together), so the fork's merge goes with it:

```bash
tools/bevy_fork.sh sync v0.20.0 --upgrade
```

Resolve our patches against the new release on `sync/v0.20.0`, then switch
the workspace's `bevy` version, the `[patch.crates-io]` block
(`tools/bevy_fork.sh cargo-patch`) and every Bevy crate we depend on in the
same matter-engine PR. That merge is also the cheapest moment to bring each
`pr/<topic>` up to date and offer it upstream. Every patch upstream takes is
one we stop carrying.

## Writing a patch

```bash
tools/bevy_fork.sh patch retained-shadows
git -C ../bevy worktree add ../bevy-retained-shadows patch/retained-shadows
```

- Keep a patch small and on its own branch. Write it the way upstream would
  take it (their code style, tests, no matter-engine names), so it can go
  upstream as `pr/<topic>`.
- Merge it into `main` with `--no-ff` by PR. The PR names the matter-engine
  spec that needs it.
- Our patches are public (the fork is public) and under Bevy's MIT /
  Apache-2.0 licences. Only engine code goes in the fork, never game
  content.

To try a patch in matter-engine before it's on `main`, point a worktree at
your local checkout in an **uncommitted** `.cargo/config.toml`:

```toml
[patch."https://github.com/poltronaut/bevy"]
bevy = { path = "../bevy-retained-shadows" }
bevy_pbr = { path = "../bevy-retained-shadows/crates/bevy_pbr" }
# ... every crate `tools/bevy_fork.sh cargo-patch` lists
```

Remove it before committing. `Cargo.lock` must keep pointing at the git
revision.

## The workspace side

The root `Cargo.toml` keeps `bevy = "0.19"` and adds the block
`tools/bevy_fork.sh cargo-patch` prints. That block covers every Bevy crate in
`Cargo.lock` that the fork holds; a missing one would leave two copies of that
crate and type errors. `Cargo.lock` pins the revision. Changing the source
rebuilds every Bevy crate once, so prune afterwards (AGENTS.md §10).

## GitHub Actions in the fork

Bevy's workflows come with the code. Today `ci.yml` runs on every pull
request (a `sync/*` or `patch/*` PR would run Bevy's full CI) and `docs.yml`
on pushes to `main`. Leave Actions off in the fork unless we add a workflow of
our own.

## Setting it up (once)

On 2026-10-08 the fork's `main` was upstream `main` with no commit of ours.

```bash
tools/bevy_fork.sh setup v0.19.1
```

This builds `fork-main` (the release plus `FORK.md`) and a local
`upstream-main`, then prints the two pushes. The second moves `main` with
`--force-with-lease`. That is safe once, because nothing of ours was on
it, and never again.
