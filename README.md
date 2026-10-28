# dependabatch

Open source dependabot PR organizer and automation.

Connects to GitHub, collects every open dependabot update PR in a repository,
squashes them into **one** conflict-free batch PR, waits for CI, merges it, then
closes the originals with a `Superseded by #N` comment.

```text
#12 Bump vue 3.4.0 -> 3.5.0        ┐
#14 Bump axios 1.6.0 -> 1.7.0      ├──>  #21 Batch PR (6 bumps, supersedes #12 #14 #16 ...)
#16 Bump zod 3.0.0 -> 4.0.0        ┘         └─ merge ──> close #12 #14 #16 as superseded
```

## Status: work in progress

| Stage | Feature | State |
| ----- | ------- | ----- |
| 1 | `dependabatch scan owner/repo`: report open dependabot PRs and conflicts | shipping |
| 2 | `dependabatch batch owner/repo`: open the batch PR | planned |
| 3 | `dependabatch merge` + supersede cleanup | planned |
| 4 | Dockerized lockfile regeneration, `dependabatch.yml` config | planned |
| 5 | GitHub Action wrapper | planned |
| 6 | Web dashboard (axum + React) | planned |
| 7 | Tauri desktop app | planned |

## Quickstart

```sh
cargo install --path crates/dependabatch-cli
dependabatch scan thiagobraga/dependabatch
```

Authentication resolution order: `GITHUB_TOKEN`, `GH_TOKEN`, then `gh auth token`.
The token needs `repo` access (Contents: read/write for the batch stage).

## Scan output

```
6 open dependabot PRs in acme/app

    PR  UPDATE                 FILES  STATUS
    12  vue 3.4.0 -> 3.5.0        2  ready
    14  axios 1.6.0 -> 1.7.0      2  ready
    18  @types/node 22.10 -> ...  2  conflict
```

- `ready`: conflicts-free, goes into the batch PR
- `conflict`: touches the same base region as another PR, stays open
- `duplicate`: same dependency grabbed by several PRs, newest wins
- `other`: grouped/security PRs dependabatch cannot safely rewrite yet

## Design decisions

- **Rust core, TS surfaces**: one core crate (lib + CLI) powers the CLI, the
  GitHub Action, the web API and the Tauri desktop app.
- **Regenerate, don't merge diffs**: the batch stage edits manifests directly and
  rebuilds lockfiles instead of stitching hunks, which keeps lockfiles consistent.
- **Belt-and-suspenders supersede**: dependabatch closes and comments on the
  original PRs itself, deterministically, right after the batch PR merges.
- **Conventional Commits** everywhere: PR titles and commit messages.

## Ecosystems (v1)

- npm (`package.json` + lockfiles via ephemeral node containers)
- GitHub Actions workflows (`.github/workflows`)

## License

MIT. See [LICENSE](LICENSE).
