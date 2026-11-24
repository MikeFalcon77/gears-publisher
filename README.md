# gears-publisher

Releases [constructorfabric/gears-rust](https://github.com/constructorfabric/gears-rust) from a
button in a web UI. It replaces the manual laptop release: `release-plz release-pr`, merge, repeated
`release-plz release` through crates.io 429s, `release_retry.py collect`, and a check against crates.io.

- Google or GitHub sign-in (Firebase). Admins are listed in the config; everyone else asks for
  access and an admin approves them.
- One workflow, `gears-release.v1`, on `cf-gears-toolkit-durable-execution`: every step is
  checkpointed in PostgreSQL, survives restarts and retries itself. A 429 waits until crates.io's
  `try again after` time. A failed run continues from the failed step with **Retry**.
- Live log over SSE, plus a per-step timeline with attempts and the next retry time.

## Workflow

It follows gears-rust's `.github/workflows/release-plz.yml` as closely as a button allows. A push to
main runs the `release` job (publish whatever crates.io lacks, retry, `collect`), then `release-pr`.
A run here does the same on the current main (`pre_release`, then `release_pr`), waits for the merge,
then does the `release` job again on the merge commit.

| Step | Does | On failure |
|---|---|---|
| `prepare` | clone if missing, `fetch --tags --force`, `checkout -B work origin/main`, `clean -fdx` | retry ×3 |
| `checks` | gears-rust's `ci.yml` without Docker (`publisher.checks`): `make fmt`, the clippy job's xtask and Python checks, `make clippy`, repo-checks, `make deny`, `make dylint` (`cargo gears lint --dylint`), `make test-no-macros`, `make test-macros`. The database lanes (`test-integration LANE=…`) are not run. Output of `publisher.condensed_checks` (default `make deny`) is logged condensed: errors and warnings with context and the end of the output, `… N lines skipped` between them | stops; merge the fix into main and press Retry: every `checks` attempt first moves the run to the current `origin/main` (logged as `main moved: …`), so Retry checks the fix, or the same commit again when main has not moved; on the same commit, checks that already passed in this run are skipped (`already passed on <sha>`, kept in `runs/<run>/checks.passed`) |
| `pre_release` | the `release` job on main: `snapshot`, `release-plz release`, `collect`. Normally publishes nothing; it finishes an interrupted earlier release before versions are computed. A failure does not stop the run (in `release-plz.yml` `release` and `release-pr` are separate jobs): a crate may be unpublishable until the release PR bumps its dependencies, and `publish` releases it after the merge | 429s: until done; other errors: logged, then on to `release_pr` |
| `release_pr` | `release-plz release-pr -o json` → PR and packages. Dry run: `release-plz update` + `git diff --stat` | retry ×3 |
| `wait_merge` | polls `gh pr view` every minute: MERGED → merge commit, CLOSED → fails | up to 14 days |
| `sync_main` | checkout the merge commit, `release_retry.py snapshot` (only the first snapshot counts) | retry ×3 |
| `publish` | the `release` job on the merge commit: `release-plz release -o json` (`--dry-run --allow-dirty` in a dry run: the tree carries `update`'s uncommitted bump) | 429s: until done; other errors: 6 attempts |
| `heal_tags` | `release_retry.py collect`: tags published crates that have no tag; full list of releases | retry ×3 |
| `verify` | every releasable workspace package: version on the crates.io sparse index, tag exists | retries for ~10 min (index lag), then fails |

Taken from `release-plz.yml`:
- `CARGO_INCREMENTAL=0` and `CARGO_REGISTRIES_CRATES_IO_PROTOCOL=sparse` for `release-plz release`.
- `~/.cargo/registry` and `~/.cargo/git` are cleared once before each release phase (the
  workaround for release-plz's gix slotmap overflow; `PUBLISHER_CLEAN_CARGO_HOME`).
- Before every retry, `release_retry.py wait`: it waits for the GitHub API budget if it is nearly
  spent and fetches tags. The minimum delay comes from the retry policy (5, then 10 minutes); a
  429 waits for crates.io's `try again after` time + 15 s instead of giving up after three attempts.
- The GitHub API budget (`gh api rate_limit`) is logged on every attempt.
- `collect` gets `OUTCOME_1=success` for a first-attempt success, otherwise `OUTCOME_1=failure`
  `OUTCOME_2=success`, so releases made by earlier attempts are recovered from tags.
- Go 1.21.13 (FIPS) and protoc 34.1 are pinned on the host; cargo-semver-checks needs both.
- dylint's hard-coded rust-clippy `master` is served from a local mirror, as ci.yml does.
- GitHub refuses anonymous reads of the organization's repositories, so git gets the `gh`
  token as an `http.extraheader` (in the command's environment, never on disk or in a log) for the
  clone, tag fetches and every `checks` command; in `checks` cargo fetches git dependencies
  with the git CLI (`CARGO_NET_GIT_FETCH_WITH_CLI`), because its libgit2 ignores that
  header (`make dylint` builds its lints from cargo-gears). release-plz authenticates with
  `GIT_TOKEN` itself. Secrets reach the build user's commands through stdin, not through
  sudo, which would log them with their values to the journal and auth.log.

Not taken: no GitHub App token (the host's `gh` login is used). Docker images and Helm charts
(`plan-publish`/`publish`) are still published only by Actions.

Each step that uses the clone first puts it at the commit the run recorded (`main` from
`prepare`, later the merge commit; in a dry run `main` plus `release-plz update`), so a step
never works on a tree another run or attempt left behind. Only one run can be active, and
Retry or Resume works only on the newest run.

gears-rust's git hooks are off in the clone (`GEARS_NO_HOOKS=1`, `gears.hooks=false`,
`core.hooksPath=/dev/null`): `release-pr` commits and pushes.

The run page opens with what happens next and who has to act: *Your turn* (merge the release
PR, fix main after failed checks, start the real release after a dry run), *Host needs fixing*
(no registry token, no GitHub login), *In progress* (including crates.io rate limits) or *Done*.
The run list marks runs that need someone.

When nothing needs releasing, the later steps do nothing and are shown as *skipped*. A dry run
never creates a PR, publishes or tags. It stops at the first crate whose dependency is not published
yet, and that counts as success.

Per-run files live in `/var/lib/gears-publisher/runs/<run-id>/`: `release-pr.json`, plus
`pre_release/` and `publish/` with `tags-before.txt`, `release-N.json`, `release-N.outcome` and
`collect-output.txt`.

The contract (step ids, timeouts and retry policies) is stored when the workflow is registered.
Changing it needs a new name (`gears-release.v2`): if you start the service with a changed contract
under the old name, it fails with `DEFINITION_CONFLICT`.

### Who sees what

Durable runs normally belong to the subject that started them. A release is shared work, so the
gear starts and controls every run as a single service identity. `publisher_runs.started_by` and the
`control` lines in the log record who started a run or pressed Cancel, Resume or Retry.

### Access

`publisher-authn-plugin` (in this workspace, derived from `cf-gears-firebase-authn-plugin`) admits
every verified Firebase user, Google or GitHub. The gear decides what each may do
(`domain/service/access_service.rs`):

| Who | How they get there | May |
|---|---|---|
| admin | `publisher.admins` in publisher.yaml | everything, and approve access requests on `/admin` |
| member | an admin approved their request | start, rerun and cancel their own PR CI (they do not see others'); follow releases read-only |
| pending | signed in and asked, with a note ("name, why") | edit the note and wait |
| rejected | an admin rejected or revoked them | nothing |
| anyone else | signed in | ask for access |

Requests live in `publisher_users`; at most 500 wait at once. Admins are not stored there and
cannot be approved, rejected or promoted in the UI.

Upgrading a host from the allowlist: in `/etc/gears-publisher/publisher.yaml`, replace the
`firebase-authn-plugin` block with `publisher-authn-plugin` (`project_id`, `tenant_id`, as in
`config/publisher.yaml`) before `deploy.sh app`; the new plugin refuses to start without them.
`publisher.users` keeps working as the admin list (`admins` is its new name).

## PR CI

The *PR CI* tab runs a pull request's CI the way gears-rust's GitHub workflows run it on a
`pull_request` event, for a PR number (or URL). Workflow `gears-pr-ci.v1`:

| Step | GitHub | Does |
|---|---|---|
| `changes` | ci.yml `changes` + checkout | `gh pr view`; fetches `refs/pull/N/{head,merge}` (refuses closed or conflicting PRs, waits while GitHub's merge commit lags the head); diffs the merge commit against its base; evaluates the path filters read from the PR's own `ci.yml` (`changes` job, `paths-ignore`) and `fmt.yml` (`paths`), or a built-in copy |
| `fmt` | fmt.yml `cargo fmt (check)` | `make fmt` |
| `repo_checks` | ci.yml `Repo checks` | gear names, PR-review rules and tests, packaging metadata, release config, release resolver |
| `security` | ci.yml `Security (cargo-deny on Ubuntu)` | `make deny` (condensed) |
| `clippy` | ci.yml `Clippy` | container pins, crypto facade, `make clippy` |
| `lint` | ci.yml `Architecture Lints` | `make dylint` |
| `test` | test-suite.yml `nextest` | `make test-no-macros` |
| `integration` | ci.yml `Integration tests` | `make test-macros`; the database lanes need Docker and are not run |
| `conclusion` | the PR's checks list | ✓/✗/– per job; fails the run if a job failed |

Each job starts from a fresh checkout of the merge commit, runs its steps until one fails and is
skipped when its `if:` filters (`rust`, `deps`, `ci`) say so. A failed job does not stop the
others. **Re-run failed jobs** and **Re-run all jobs** start a new run on the PR's current head
(re-run jobs ignore the filters); **Retry** continues a run that stopped before the jobs (e.g.
after resolving a merge conflict). As with GitHub's `concurrency` group, a new run of the same
PR replaces your run in progress or in the queue (an admin's replaces anyone's). Another PR's
CI waits in a queue (`publisher_ci_queue`, shown on the CI page) and starts by itself once the
run in progress settles; a queued request can be cancelled before it starts.

PR CI and the release share the host: one heavy build runs at a time (release `checks`,
`release-pr`, `release-plz release`, each CI job); the others wait and say so in their log.
A waiting release build goes before PR CI, so a release between `release-plz release`
attempts does not wait behind a PR's tests. Builds run with `CARGO_INCREMENTAL=0`, as in
gears-rust's CI. Each build checks the disk first: below `min_free_disk_gb` (default 130 GiB; a full
test rebuild writes about 112) free it removes its own `target/debug` and starts cold.

Not run: Docker lanes (database integration, e2e), macOS/Windows, FIPS, coverage, CodeQL, CFS.
Results are not posted to GitHub.

## Layout

```
crates/publisher-server   binary: gears bootstrap + api-gateway, authn, firebase plugin, types-registry
crates/publisher          the gear, layered like gears-rust's gears:
  src/api/rest              routes, handlers, DTOs, problem errors, SSE, static UI
  src/domain                models, errors, ports, repository trait, run service,
                            release/ (gears-release.v1), ci/ (gears-pr-ci.v1), shared steps
  src/infra                 PostgreSQL (entities, migrations, repository), subprocesses, crates.io
  src/gear.rs, config.rs    wiring and configuration
web/                      React 19 + Vite + Tailwind UI
config/publisher.yaml     application config (admins, paths)
deploy/                   provisioning, systemd, Caddy, sudo rules, deploy script
```

## Development

Requirements: Rust 1.98.1, Node 24, Docker (PostgreSQL), `protoc`. Locally `run_as` is empty and
every command runs as you.

```sh
docker run -d --name gears-publisher-pg -e POSTGRES_USER=publisher -e POSTGRES_PASSWORD=publisher \
  -e POSTGRES_DB=publisher -p 55433:5432 postgres:16
cp .env.example .env            # set FIREBASE_PROJECT_ID
cp web/.env.example web/.env.local   # Firebase web config
(cd web && npm ci && npm run build)
set -a; . ./.env; set +a; cargo run -p publisher-server
# UI with hot reload: (cd web && npm run dev) → http://127.0.0.1:5173, proxies /publisher to :8090
```

Checks:

```sh
cargo clippy --all-targets -- -D warnings
cargo test                                   # unit tests + workflow tests on TestRuntime
(cd web && npm test && npm run build)
# The real engine on PostgreSQL (restart in the middle of `checks`); needs a disposable database:
PUBLISHER_TEST_PG_URL=postgres://publisher:publisher@127.0.0.1:55433/publisher_test \
  cargo test -p publisher --test pg_engine -- --ignored
```

## Deployment

Everything runs on one Ubuntu 24.04 virtual machine. You run the commands below from your
laptop, in this repository. `deploy/deploy.sh` connects to the machine as `root` over SSH,
copies the sources there and runs the scripts. Nothing secret is stored in this repository:
tokens, passwords and the list of admins live only on the machine.

### What you need

- **A virtual machine** with Ubuntu 24.04 (x86-64), a public IP, and SSH access as `root`
  with a key. Size: at least 16 GiB of memory and 250 GiB of free disk. gears-rust's tests are
  big: one full test build writes about 112 GiB, and the release and PR CI each keep their own.
  More CPUs make builds faster. Open TCP ports 22, 80 and 443.
- **A Firebase project** for sign-in (free plan is enough).
- **A GitHub account** that may push branches to gears-rust and open pull requests. The
  release PRs are opened as this account.
- **A crates.io API token** with permission to publish the gears crates.
- On the laptop: `bash`, `ssh`, `rsync`, and the [Firebase CLI](https://firebase.google.com/docs/cli).

`deploy.sh` uses the SSH key `~/.ssh/id_ed25519_gears_publisher` if it exists; set
`PUBLISHER_SSH_KEY` to use another one. Below, `<ip>` is the machine's public IP.

### 1. Set up Firebase

```sh
firebase login
firebase projects:create <project-id> --display-name gears-publisher
firebase apps:create WEB "gears-publisher web" --project <project-id>
firebase apps:sdkconfig WEB --project <project-id>
```

Copy `web/.env.example` to `web/.env.local` and fill it from the `sdkconfig` output
(`apiKey`, `authDomain`, `projectId`). These values are public: they end up in the web page.
`web/.env.local` is not committed.

Turn on sign-in in the Firebase Console → Authentication → Sign-in method:

- **Google**: enable it.
- **GitHub** (optional): create a GitHub OAuth App (GitHub → Settings → Developer settings →
  OAuth Apps) with the callback URL `https://<project-id>.firebaseapp.com/__/auth/handler`,
  then enable GitHub in Firebase with that app's client id and secret.

Allow the site's address: Firebase Console → Authentication → Settings → Authorized domains →
add `<ip-with-dashes>.sslip.io` (for example `203-0-113-7.sslip.io` for `203.0.113.7`).
`deploy/firebase-domain.mjs` does the same from the command line (see the comment at its top).

### 2. Provision the machine

```sh
FIREBASE_PROJECT_ID=<project-id> GIT_EMAIL=<github-account-email> deploy/deploy.sh provision <ip>
```

This takes a while the first time (it builds the Rust tools). It is safe to run again at any
time: it only fixes what is missing or different. It does all of this:

- Installs system packages, PostgreSQL 16, Caddy (HTTPS), the GitHub CLI, Node 24, Go 1.21.13
  and protoc. Every download is pinned to a version and checked against a sha256 sum.
- Creates three users:
  - `publisher` runs the service. Only it can read the tokens and the database password.
  - `publisher-build` runs gears-rust's code for releases (checks, release-plz).
  - `publisher-ci` runs pull requests' code for PR CI. It cannot write anything the
    release uses.
- Gives each user its own Rust toolchain (1.98.1, with clippy, rustfmt and rust-src), and
  installs the cargo tools (release-plz, cargo-gears, nextest, semver-checks, cargo-deny,
  cargo-hack) and a Python venv, all owned by root so the build users cannot change them.
- Creates the directories with the right owners and permissions. These are the fixes for
  problems we hit when we first ran CI on the machine:
  - each build user gets its own temp directory, so a file one user leaves in `/tmp` cannot
    break the other's tests;
  - `publisher-ci`'s home is `0751`, so the service can start commands in it;
  - `publisher-ci` trusts the service's PR mirror (`git safe.directory`), so cloning a PR
    does not fail with "dubious ownership";
  - git serves rust-clippy's `master` from a local mirror, as gears-rust's `ci.yml` does
    (`make dylint` needs it);
  - the DNS search domain is removed, so names that do not exist (gears-rust's tests use
    `unreachable.invalid`) fail to resolve, as they should. Some hosting providers answer
    every name under their own domain.
- Writes the sudo rules that let the service run commands as the two build users
  (`deploy/sudoers`).
- Sets the git name and email of all three users to `GIT_EMAIL`; release-plz commits with it.
- Creates the database with a random password and writes `/etc/gears-publisher/publisher.env`.
  `CARGO_JOBS` (parallel build jobs) is chosen from the machine's memory and CPUs; set
  `CARGO_JOBS=<n>` to choose it yourself.
- Configures Caddy for `https://<ip-with-dashes>.sslip.io` with a Let's Encrypt certificate.
- Allows SSH with keys only and turns off rpcbind.
- Installs the systemd service `gears-publisher`.

At the end it lists what is still missing. The next steps cover it.

### 3. Log in to GitHub on the machine

The service uses this login to clone gears-rust, push the release branch, open the release PR
and push tags. Use a fine-grained token limited to gears-rust (Contents: read and write, Pull
requests: read and write), not your personal login:

```sh
ssh root@<ip>
sudo -iu publisher gh auth login --with-token     # paste the token, then Ctrl-D
```

### 4. Copy the tokens

Put the tokens in a local file (never commit it), for example `~/gears-publisher.env`:

```sh
CARGO_REGISTRY_TOKEN=cio...
# Optional, for PR CI: a fine-grained token with read-only access to repository contents.
# PR checks use it to download git dependencies from GitHub. Without it they fetch anonymously.
GITHUB_CI_TOKEN=github_pat_...
```

```sh
deploy/deploy.sh secrets <ip> ~/gears-publisher.env
```

Only these two lines are copied, through SSH's input (not the command line), into
`/etc/gears-publisher/secrets.env` (readable by `publisher` only).

### 5. Build and start the app

```sh
deploy/deploy.sh app <ip>
```

This builds the web UI and the server on the machine, installs them and restarts the service.
Run the same command to deploy every later change. Open `https://<ip-with-dashes>.sslip.io`.

### 6. Add the admins

Admins start releases and approve everyone else. Sign in to the site once, then find your
user id in the Firebase Console → Authentication → Users ("User UID"). Make a local copy of
the config (`config/*.local.yaml` is not committed):

```sh
cp config/publisher.yaml config/publisher.local.yaml
# edit publisher.admins in config/publisher.local.yaml: uid and email of each admin
deploy/deploy.sh config <ip>
```

This installs the file as `/etc/gears-publisher/publisher.yaml` and restarts the service. It
replaces the whole file, so always edit your local copy, not the file on the machine.
Everyone else signs in, asks for access, and an admin approves them on `/admin` (no restart).

### Updating

- Code changes: `deploy/deploy.sh app <ip>`.
- Changes in `deploy/` (packages, users, permissions, tool versions): `deploy/deploy.sh
  provision <ip>`, then `deploy/deploy.sh app <ip>`.
- Config changes: `deploy/deploy.sh config <ip>`.

Restarting the service in the middle of a step runs that step again from the beginning. That is
safe for every step: publishing skips crates that are already published. Deploying during a long
`checks` step costs the build time. The interrupted attempt counts against the step's retry limit;
the limits leave room for that.

### PR CI and the cargo-gears fork

GitHub has flagged the constructorfabric organization, so its public repositories refuse
anonymous git. `make dylint` downloads cargo-gears from there, so PR CI downloads it from a
public fork instead (`ci_git_rewrites` in `config/publisher.yaml`). The fork must have every
`cargo-gears-v*` tag that gears-rust pins; when gears-rust moves to a new cargo-gears tag, push
that tag to the fork. Remove the rewrite once the organization is unflagged.

## Security

- Every `/publisher/v1/*` route, the SSE stream included, needs a Firebase ID token for this
  project; anything else answers 401. A valid token alone gets only `/me` and the access request:
  every run route answers 403 (`not_registered`, `pending_approval`, `rejected`) unless the caller
  is an admin or an approved member, and release controls answer 403 `admin_only` to members.
  A member's PR CI runs and queue entries are theirs alone: anyone else's answer 404, as if
  they did not exist (runs record the starter's subject id in `publisher_runs.started_by_subject`).
  Checked on the live host: no token, a garbage token, `alg: none`, RS256 signed with a foreign key
  under Google's real `kid`, and HS256 are all refused. So are `//`, `..`, `%2F` and other path variants.
- Caddy proxies only `/`, `/index.html`, `/favicon.svg`, `/ci`, `/admin`, `/runs/*`, `/assets/*` and `/publisher/v1/*`.
  types-registry REST, `/health`, `/readyz` and OpenAPI stay on 127.0.0.1. HSTS is on.
- Listening on the public IP: 22 (SSH), 80 and 443 (Caddy). The service (8080), PostgreSQL
  (5432) and Caddy's admin API (2019) listen on loopback only; rpcbind is disabled.
- SSH accepts keys only (`deploy/sshd-hardening.conf`).
- Everything that runs gears-rust's code (checks, release-plz, `release_retry.py`, git in the
  clone) runs as `publisher-build` through `sudo` (`deploy/sudoers`). That user cannot read
  `/etc/gears-publisher` (`secrets.env` is `root:publisher 0640`), the service's home with the
  gh login, or the database password, and cannot change the root-owned binaries, UI, tools and
  venv in `/opt/gears-publisher`. Only `gh` runs as the service.
- Commands start from an empty environment: a short allowlist (`PATH`, `HOME`, …), the
  configured `env` and the command's own variables. `CARGO_REGISTRY_TOKEN` and the gh token
  are passed only to the commands that need them (`release-plz release`, `release-pr`,
  `update` and `release --dry-run` in a dry run (the changelog template and the release-PR lookup read GitHub), `release_retry.py wait/collect`), never as arguments, and are masked in the log.
- PR CI runs a pull request's code as `publisher-ci`, which is not in `publisher-build`'s
  group: it cannot write the release's clone, target directory or run files (a planted
  `.git/config` there would run with the gh token at the next release). The PR is fetched into
  the service's bare mirror (`/var/lib/gears-publisher-mirror`) with the gh token, by git
  running as the service; the CI clone fetches from that mirror without credentials. Checks get
  only `GITHUB_CI_TOKEN`. With `run_as` set and no `ci_run_as`, PR CI is refused.
- Each build user has its own `TMPDIR` (`<work_dir>/tmp`, `<ci_work_dir>/tmp`, 0700): under
  the service's `PrivateTmp` both share one `/tmp`, where a fixed path made by one user is not
  writable by the other (toolkit-db's `cf_gears_test_locks`) and PR code could leave files a
  release then uses.
- What remains: those commands build crates (`cargo publish` verifies), so a malicious
  dependency could read the two tokens from its environment during a release, as in GitHub
  Actions. With a personal `gh` login that token reaches all of its owner's repositories; a
  fine-grained token for gears-rust only limits it (Deployment, step 3).
- The UI sends a Content-Security-Policy (own scripts, Firebase sign-in only).
- The `sslip.io` name belongs to whoever holds the IP. If the IP is released, remove it from
  Firebase's authorized domains; a domain of your own avoids this.

## Operations

- `deploy/deploy.sh status <ip>`: service state and recent log. `journalctl -u gears-publisher -f` on the host.
- Memory: if the checks hit OOM, lower `PUBLISHER_CARGO_JOBS` in `/etc/gears-publisher/publisher.env`.
- Logs of runs older than 180 days are deleted daily; the runs stay listed.
- The live log stream ends when a run settles and reconnects every 10 minutes with a fresh token.
- The `sslip.io` name follows the IP. If the IP changes, update Firebase's authorized domains and
  re-run `provision` (it regenerates the Caddyfile).
- Docker images and Helm charts of gears are still published only by GitHub Actions.
