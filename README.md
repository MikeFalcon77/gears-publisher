# gears-publisher

Releases [constructorfabric/gears-rust](https://github.com/constructorfabric/gears-rust) from a
button in a web UI. It replaces the manual laptop release: `release-plz release-pr`, merge, repeated
`release-plz release` through crates.io 429s, `release_retry.py collect`, and a check against crates.io.

- Google sign-in (Firebase). Only accounts in `config/publisher.yaml` get in.
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
| `checks` | gears-rust's `ci.yml` without Docker (`publisher.checks`): `make fmt`, the clippy job's xtask and Python checks, `make clippy`, repo-checks, `make deny`, `make dylint` (`cargo gears lint --dylint`), `make test-no-macros`, `make test-macros`. The database lanes (`test-integration LANE=…`) are not run. Output of `publisher.condensed_checks` (default `make deny`) is logged condensed: errors and warnings with context and the end of the output, `… N lines skipped` between them | stops; fix main and start a new release (Retry checks the same commit: only for flaky or host problems) |
| `pre_release` | the `release` job on main: `snapshot`, `release-plz release`, `collect`. Normally publishes nothing; it finishes an interrupted earlier release before versions are computed | like `publish` |
| `release_pr` | `release-plz release-pr -o json` → PR and packages. Dry run: `release-plz update` + `git diff --stat` | retry ×3 |
| `wait_merge` | polls `gh pr view` every minute: MERGED → merge commit, CLOSED → fails | up to 14 days |
| `sync_main` | checkout the merge commit, `release_retry.py snapshot` (only the first snapshot counts) | retry ×3 |
| `publish` | the `release` job on the merge commit: `release-plz release -o json` (`--dry-run` in a dry run) | 429s: until done; other errors: 6 attempts |
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
gear starts and controls every run as a single service identity. The allowlist is enforced by
`firebase-authn-plugin`, before a request reaches the gear. `publisher_runs.started_by` and the
`control` lines in the log record who started a run or pressed Cancel, Resume or Retry.

## Layout

```
crates/publisher-server   binary: gears bootstrap + api-gateway, authn, firebase plugin, types-registry
crates/publisher          the gear: config, auth, workflow, runner, store, REST/SSE, static UI
web/                      React 19 + Vite + Tailwind UI
config/publisher.yaml     application config (allowlist, paths)
deploy/                   provisioning, systemd, Caddy, deploy script, Virtuozzo spec
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

## Deployment (Virtuozzo, eu1)

Current instance: environment `gears-publisher`, node <id>, IP 203.0.113.7,
<https://203-0-113-7.sslip.io>. Firebase project `<project-id>`.

Size: 8 fixed cloudlets (1 GiB, always reserved: service + PostgreSQL) and 128 flexible (the
account's per-node ceiling: 16 GiB, 20 vCPU), paid by use. The checks and `cargo publish` burst
into the flexible part; the rest of the time the host is idle and waits for merges.

1. **Container.** `createenvironment` refuses `region`, `extip` and `diskLimit` together
   (API result 2395), so create it minimal and size it afterwards:
   ```sh
   vap env create --name gears-publisher --input deploy/spec.json --yes --wait
   vap api call environment/control/rest/setcloudletscount --param envName=gears-publisher \
     --param nodeType=ubuntu-vps --param fixedCloudlets=8 --param flexibleCloudlets=128 --yes
   vap api call environment/control/rest/attachextip --param envName=gears-publisher --param nodeid=<id> --yes
   ```
   The disk defaults to the 400 GB maximum. Open TCP ports 80 and 443 with `environment/security/rest/addrule`.
   SSH: add the public key with `management/account/rest/addsshkey`. It reaches the node within
   about a minute (`execcmdbyid` does not work on VPS nodes: 4109). `deploy.sh` uses
   `~/.ssh/id_ed25519_gears_publisher` (override with `PUBLISHER_SSH_KEY`).
2. **Firebase** (CLI, logged in):
   ```sh
   firebase projects:create <id> --display-name gears-publisher
   firebase apps:create WEB "gears-publisher web" --project <id>
   firebase apps:sdkconfig WEB --project <id>          # → web/.env.local (VITE_FIREBASE_*)
   # Google sign-in: firebase.json {"auth":{"providers":{"googleSignIn":{"oAuthBrandDisplayName":…,"supportEmail":…}}}}
   firebase deploy --only auth --project <id>
   node deploy/firebase-domain.mjs <id> <ip-with-dashes>.sslip.io
   ```
3. **Provision:** `FIREBASE_PROJECT_ID=<id> deploy/deploy.sh provision <ip>`. This installs the
   packages, Go, protoc, gh, Node 24, PostgreSQL 16, Caddy, the `publisher` and `publisher-build`
   users with their own rustup, the cargo tools (release-plz, cargo-gears, nextest,
   semver-checks, cargo-deny, cargo-hack) and the Python venv. Every download and tool version
   is pinned; tarballs and `rustup-init` are checked against sha256 sums, pyyaml with
   `--require-hashes`. It also creates the database, the sudo rule, the Caddyfile and the
   systemd unit.
4. **Secrets:** `deploy/deploy.sh secrets <ip> ~/projects/gears-rust/.env` copies only
   `CARGO_REGISTRY_TOKEN`, over stdin, into `/etc/gears-publisher/secrets.env` (0600).
5. **GitHub** (manual, on the host): `sudo -iu publisher gh auth login` and
   `git config --global user.email` for that account (provision copies the email to the build
   user, whose commits release-plz pushes with `GIT_TOKEN`). Better than a personal login: a
   fine-grained token for gears-rust only (contents and pull requests: read and write), as
   `gh auth login --with-token`.
6. **App:** `deploy/deploy.sh app <ip>` builds the UI and the release binary on the host (as
   `publisher`), installs them root-owned and restarts the service.
7. **Access:** each person signs in once. The *Access denied* page shows their uid and email; add
   them under `firebase-authn-plugin.identities` in `/etc/gears-publisher/publisher.yaml` (or in
   `config/publisher.yaml`, then `deploy/deploy.sh config <ip>`), then restart.

Restarting the service in the middle of a step runs that step again from the beginning. That is safe
for every step: publishing skips crates that are already published. Deploying during a long `checks`
step costs the build time. The interrupted attempt counts against the step's retry limit; the
limits leave room for that (failed checks and repeated publish errors stop the run on their own).

## Security

- Every `/publisher/v1/*` route, the SSE stream included, needs a Firebase ID token. The token
  must be for an account in `firebase-authn-plugin.identities` (uid and verified email), otherwise
  the API answers 401. A subject the gear itself does not know gets 403.
  Checked on the live host: no token, a garbage token, `alg: none`, RS256 signed with a foreign key
  under Google's real `kid`, and HS256 are all refused. So are `//`, `..`, `%2F` and other path variants.
- Caddy proxies only `/`, `/index.html`, `/favicon.svg`, `/runs/*`, `/assets/*` and `/publisher/v1/*`.
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
  `release_retry.py wait/collect`), never as arguments, and are masked in the log.
- What remains: those commands build crates (`cargo publish` verifies), so a malicious
  dependency could read the two tokens from its environment during a release, as in GitHub
  Actions. With a personal `gh` login that token reaches all of its owner's repositories; a
  fine-grained token for gears-rust only limits it (step 5).
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
