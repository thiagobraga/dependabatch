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
| 1 | `dependabatch scan [owner/repo\|path]`: report open dependabot PRs and conflicts | shipping |
| 2 | `dependabatch run [owner/repo\|path]`: open the batch PR (aliases: `pr`, `fix`, `batch`) | shipping |
| 3 | `dependabatch merge` / `dependabatch close`: merge and close superseded PRs | shipping |
| 4 | `dependabatch.yml` per-repo config | planned |
| 5 | GitHub Action wrapper (`uses: thiagobraga/dependabatch@v0.5.0`) | shipping |
| 6 | Web dashboard (axum + React) | planned |
| 7 | Tauri desktop app | planned |

## Quickstart

```sh
cargo install --path crates/dependabatch-cli   # installs `dependabatch` + `dbatch`
# inside a checkout: scans the GitHub repo behind its git remote
cd ~/projects/app && dependabatch scan
# or point at a local checkout / an explicit repo
dbatch scan ~/projects/app
dbatch scan thiagobraga/dependabatch
# scan GitHub code-scanning alerts (table + path breakdown)
dbatch scan -a thiagobraga/planner
dbatch alerts thiagobraga/planner --severity critical
dbatch alerts thiagobraga/planner --alert 191
# preview or open a PR fixing code-scanning alerts
dbatch fix -a thiagobraga/planner -n
dbatch fix --alert 216 thiagobraga/planner
# look at the plan without touching anything
dependabatch run owner/repo --dry-run
# open the batch PR (or use aliases: dbatch pr, dbatch fix, dbatch batch)
dependabatch run owner/repo
# merge the open batch PR and close superseded dependabot PRs
dependabatch merge owner/repo
# or close superseded PRs after merging on GitHub
dependabatch close owner/repo
# local dashboard in your browser
dependabatch serve --host 127.0.0.1 --port 8280
```

`scan` and `run` (or `pr`/`fix`/`batch`) take an optional repository: `owner/name`, a GitHub URL, or
a local path. Without one they use the current directory. Local paths resolve
through the checkout's `origin` remote (falling back to the first remote).
`dbatch` is a short alias for `dependabatch`.

Authentication resolution order: `GITHUB_TOKEN`, `GH_TOKEN`, then `gh auth token`.
The token needs `repo` access (Contents: read/write for the batch stage).

## Install

One-liner (linux + macOS, installs `dependabatch` plus a `dbatch` symlink to `~/.local/bin`):

```sh
curl -fsSL https://raw.githubusercontent.com/thiagobraga/dependabatch/main/install.sh | sh
```

Environment overrides: `DEPENDABATCH_VERSION=v0.1.0` pins a tag,
`DEPENDABATCH_INSTALL=/usr/local/bin` changes the target dir. The script
detects the platform, downloads the release asset, verifies its sha256 and
prints the binary version at the end. The linux x86_64 build is static (musl),
so it runs on any distro. Windows or other platforms: grab an asset from
[releases](https://github.com/thiagobraga/dependabatch/releases) directly, or
install from a checkout with `cargo install --path crates/dependabatch-cli`.

## Batch flags

| Flag | Meaning |
| ---- | ------- |
| `-n` / `--dry-run` | print the plan (in/out per PR) plus a preview of the batch PR, then stop |
| `--exclude zod` | keep a dependency out of the batch |
| `--base main` | build from another base branch instead of the default |
| `--regen-image node:24` | docker image used for lockfile regen |
| `--no-regen` | skip lockfile regeneration (manifest edits only) |

The batch PR body carries a table of every bump plus `Close #12, Close #14`,
so GitHub cross-links the originals and `dependabatch close` / `dependabatch merge`
knows which PRs to close.

## Code-scanning alert flags

Use `dependabatch scan --alerts` (or `-a`, or the `dependabatch alerts` / `dbatch cs` alias)
to query GitHub's code-scanning alerts API with automatic pagination and path grouping:

| Flag | Meaning |
| ---- | ------- |
| `-a` / `--alerts` | query code-scanning alerts on `scan`, or fix them on `fix` / `run` (alias: `--code-scanning`) |
| `--severity <level>` | filter by severity (`critical`, `high`, `medium`, `low`, `error`, `warning`, `note`) |
| `--state <state>` | alert state: `open` (default), `fixed`, `dismissed`, or `closed` |
| `--ref <git-ref>` | branch or git ref (defaults to repo default branch; pass `all` for all refs) |
| `--alert <number>` | inspect (`scan` / `alerts`) or fix (`fix` / `alerts --fix`) a single alert |
| `--path <substring>` | filter alerts by file/target path when fixing |
| `--ai <mode>` | AI CLI fallback (`auto`, `claude`, `gemini`, `none`) for source/container fixes |
| `--json` | output machine-readable JSON |

## GitHub Action

Run dependabatch directly in GitHub Actions on a schedule or dispatch, and
automatically close superseded PRs when the batch PR merges:

```yaml
name: dependabatch
on:
  schedule:
    - cron: '0 6 * * 1-5'
  workflow_dispatch:
  pull_request:
    types: [closed]

jobs:
  batch:
    if: github.event_name != 'pull_request'
    runs-on: ubuntu-latest
    permissions:
      contents: write
      pull-requests: write
    steps:
      - uses: actions/checkout@v4
      - uses: thiagobraga/dependabatch@v0.4.0
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
          command: run

  close-superseded:
    if: >-
      github.event_name == 'pull_request' &&
      github.event.pull_request.merged == true &&
      github.event.pull_request.head.ref == 'dependabatch/batch'
    runs-on: ubuntu-latest
    permissions:
      issues: write
      pull-requests: write
    steps:
      - uses: actions/checkout@v4
      - uses: thiagobraga/dependabatch@v0.4.0
        with:
          token: ${{ secrets.GITHUB_TOKEN }}
          command: close
          pr: ${{ github.event.pull_request.number }}
```

## Docker cleanup

Lockfile regeneration runs in an ephemeral node container started with `--rm`,
so containers disappear when a run ends, even on failure. Leftovers only appear
if you kill the docker daemon mid-run. Audit and clean:

```sh
# audit
docker ps -a --filter name=dependabatch-regen

# remove stragglers
docker rm -f $(docker ps -aq --filter name=dependabatch-regen)

# free up the pulled image when you want the npm cache gone too
docker image rm node:24
docker image prune -f
```

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
