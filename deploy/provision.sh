#!/usr/bin/env bash
# Prepares a fresh Ubuntu 24.04 VPS for gears-publisher. Idempotent; runs as root from
# the synced sources (/opt/gears-publisher/src): use `deploy/deploy.sh provision <ip>`.
# Optional environment: FIREBASE_PROJECT_ID, PUBLIC_IP (detected otherwise).
#
# Two users:
#   publisher        the service: GitHub login (gh), CARGO_REGISTRY_TOKEN, database.
#                    Builds the service itself from /opt/gears-publisher/src.
#   publisher-build  runs everything that executes gears-rust's code (checks,
#                    release-plz, release_retry.py) through sudo. It cannot read the
#                    secrets, the service's home or its binaries; it gets the tokens
#                    only as environment of the commands that need them.
#   publisher-ci     runs PR CI: a pull request's code. It cannot write anything of
#                    the release (not in publisher-build's group) and gets no token
#                    but the optional read-only GITHUB_CI_TOKEN.
# Binaries, the UI, tools and the Python venv are root-owned.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

APP=/opt/gears-publisher
WORK=/var/lib/gears-publisher
BUILD_HOME=/var/lib/gears-publisher-build
CI_HOME=/var/lib/gears-publisher-ci
# The service's bare mirror PR CI fetches from; outside $WORK, which is chowned below.
MIRROR=/var/lib/gears-publisher-mirror
ETC=/etc/gears-publisher
RUST_TOOLCHAIN=1.98.1

# Downloads are pinned and checked against these sha256 sums.
RUSTUP_VERSION=1.29.3
RUSTUP_SHA256=b1c8cb14e30889e7e2999e6b69e62d9e9b9ffc0f4df8180564d306d9680e6c55
NODE_VERSION=24.22.1
NODE_SHA256=6c172a9ab6718f7b801fb5a7bd486af2ee5e32758086a04b84fcce2aa77095e6
# Like release-plz.yml: Go 1.21.x (aws-lc-fips-sys, see gears-rust
# docs/security/fips/TOOLCHAIN.md) and protoc 3.34.1 (prost-build, semver-checks).
GO_VERSION=1.21.13
GO_SHA256=502fc16d5910562461e6a6631fb6377de2322aad7304bf2bcd23500ba9dab4a7
PROTOC_VERSION=34.1
PROTOC_SHA256=af27ea66cd26938fe48587804ca7d4817457a08350021a1c6e23a27ccc8c6904
PYYAML_REQUIREMENT="pyyaml==6.0.3 --hash=sha256:ba1cc08a7ccde2d2ec775841541641e4548226580ab850948cbfda66a1befcdc"
# Cargo tools (`cargo install --locked`), as in release-plz.yml and gears-rust's ci.yml.
CARGO_TOOLS=(
  release-plz@0.4.3
  cargo-gears@0.0.8
  cargo-nextest@0.9.151
  cargo-semver-checks@0.51.0
  cargo-deny@0.20.2
  cargo-hack@0.6.46
)

log() { printf '\n==> %s\n' "$*"; }

# fetch <url> <sha256> <file>
fetch() {
  curl -fsSL --retry 3 -o "$3" "$1"
  echo "$2  $3" | sha256sum -c --quiet - || { echo "checksum mismatch: $1" >&2; exit 1; }
}

log "packages"
apt-get update -q
apt-get install -yq --no-install-recommends \
  build-essential clang cmake pkg-config libssl-dev \
  git curl jq rsync unzip xz-utils ca-certificates gnupg sudo \
  debian-keyring debian-archive-keyring apt-transport-https \
  python3 python3-venv python3.12-venv \
  postgresql-16 postgresql-contrib

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [[ "$(/usr/local/go/bin/go version 2>/dev/null)" != *"go$GO_VERSION "* ]]; then
  log "Go $GO_VERSION"
  fetch "https://go.dev/dl/go$GO_VERSION.linux-amd64.tar.gz" "$GO_SHA256" "$tmp/go.tgz"
  rm -rf /usr/local/go && tar -C /usr/local -xzf "$tmp/go.tgz"
fi
if [[ "$(/usr/local/bin/protoc --version 2>/dev/null)" != *"$PROTOC_VERSION"* ]]; then
  log "protoc $PROTOC_VERSION"
  fetch "https://github.com/protocolbuffers/protobuf/releases/download/v$PROTOC_VERSION/protoc-$PROTOC_VERSION-linux-x86_64.zip" \
    "$PROTOC_SHA256" "$tmp/protoc.zip"
  unzip -oq "$tmp/protoc.zip" -d /usr/local bin/protoc 'include/*'
  chmod 755 /usr/local/bin/protoc
fi

if [[ "$(/usr/local/bin/node --version 2>/dev/null)" != "v$NODE_VERSION" ]]; then
  log "Node $NODE_VERSION"
  # Replaces an earlier NodeSource install.
  if dpkg -s nodejs >/dev/null 2>&1; then apt-get remove -yq nodejs; fi
  rm -f /etc/apt/sources.list.d/nodesource.list /etc/apt/sources.list.d/nodesource.sources
  fetch "https://nodejs.org/dist/v$NODE_VERSION/node-v$NODE_VERSION-linux-x64.tar.xz" "$NODE_SHA256" "$tmp/node.txz"
  rm -rf /usr/local/lib/nodejs && mkdir -p /usr/local/lib/nodejs
  tar -C /usr/local/lib/nodejs --strip-components=1 -xJf "$tmp/node.txz"
  for b in node npm npx; do ln -sf "/usr/local/lib/nodejs/bin/$b" "/usr/local/bin/$b"; done
fi

# gh and Caddy come from their signed apt repositories.
if ! command -v gh >/dev/null; then
  log "GitHub CLI"
  install -dm 755 /etc/apt/keyrings
  curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
    -o /etc/apt/keyrings/githubcli-archive-keyring.gpg
  chmod go+r /etc/apt/keyrings/githubcli-archive-keyring.gpg
  echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
    > /etc/apt/sources.list.d/github-cli.list
  apt-get update -q && apt-get install -yq gh
fi
if ! command -v caddy >/dev/null; then
  log "Caddy"
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
    | gpg --dearmor --yes -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
    > /etc/apt/sources.list.d/caddy-stable.list
  apt-get update -q && apt-get install -yq caddy
fi

log "users and directories"
id publisher >/dev/null 2>&1 || useradd --create-home --shell /bin/bash publisher
id publisher-build >/dev/null 2>&1 \
  || useradd --system --create-home --home-dir "$BUILD_HOME" --shell /usr/sbin/nologin publisher-build
# The service writes per-run files that the build user's commands write into too.
usermod -aG publisher-build publisher
chmod 0700 /home/publisher
chmod 0750 "$BUILD_HOME"
install -d -o root -g root -m 0755 "$APP" "$APP/bin" "$APP/tools" "$APP/tools/bin"
install -d -o publisher -g publisher -m 0755 "$APP/src"
install -d -o publisher-build -g publisher-build -m 2775 "$WORK"
chown -R publisher-build:publisher-build "$WORK"
install -d -o publisher -g publisher-build -m 2775 "$WORK/runs"
id publisher-ci >/dev/null 2>&1 \
  || useradd --system --create-home --home-dir "$CI_HOME" --shell /usr/sbin/nologin publisher-ci
chmod 0750 "$CI_HOME"
install -d -o publisher-ci -g publisher-ci -m 0755 "$CI_HOME/work"
install -d -o publisher -g publisher -m 0755 "$MIRROR"
install -d -o root -g publisher -m 0750 "$ETC"
if [[ -f "$ETC/secrets.env" ]]; then
  chown root:publisher "$ETC/secrets.env" && chmod 0640 "$ETC/secrets.env"
else
  install -o root -g publisher -m 0640 /dev/null "$ETC/secrets.env"
fi

log "sudo rules for the build users"
install -o root -g root -m 0440 "$APP/src/deploy/sudoers" "$tmp/sudoers"
visudo -cqf "$tmp/sudoers"
install -o root -g root -m 0440 "$tmp/sudoers" /etc/sudoers.d/gears-publisher
rm -f /etc/sudoers.d/gears-publisher-test

# install_rustup <user> <home>
install_rustup() {
  if [[ ! -x "$2/.cargo/bin/rustup" ]]; then
    fetch "https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/x86_64-unknown-linux-gnu/rustup-init" \
      "$RUSTUP_SHA256" "$tmp/rustup-init"
    chmod 755 "$tmp/rustup-init"
    sudo -u "$1" HOME="$2" "$tmp/rustup-init" -y --no-modify-path --profile minimal \
      --default-toolchain "$RUST_TOOLCHAIN"
  fi
  sudo -u "$1" HOME="$2" "$2/.cargo/bin/rustup" toolchain install "$RUST_TOOLCHAIN" \
    --profile minimal --component clippy,rustfmt,rust-src
  sudo -u "$1" HOME="$2" "$2/.cargo/bin/rustup" default "$RUST_TOOLCHAIN"
}

log "Rust for the service (publisher) and for builds (publisher-build, publisher-ci)"
chmod 755 "$tmp"
install_rustup publisher /home/publisher
install_rustup publisher-build "$BUILD_HOME"
install_rustup publisher-ci "$CI_HOME"

log "cargo tools (built as publisher, installed root-owned)"
for tool in "${CARGO_TOOLS[@]}"; do
  name=${tool%@*}; version=${tool#*@}
  if ! grep -q "^\"$name $version " /home/publisher/tools/.crates.toml 2>/dev/null; then
    sudo -u publisher HOME=/home/publisher \
      PATH=/home/publisher/.cargo/bin:/usr/local/go/bin:/usr/local/bin:/usr/bin:/bin \
      /home/publisher/.cargo/bin/cargo install --locked --root /home/publisher/tools "$tool"
  fi
done
install -o root -g root -m 0755 /home/publisher/tools/bin/* "$APP/tools/bin/"

log "Python venv for release_retry.py and the repo checks"
if [[ -d "$APP/venv" && "$(stat -c %U "$APP/venv")" != root ]]; then rm -rf "$APP/venv"; fi
[[ -x "$APP/venv/bin/python" ]] || python3 -m venv "$APP/venv"
echo "$PYYAML_REQUIREMENT" > "$tmp/requirements.txt"
"$APP/venv/bin/pip" install -q --disable-pip-version-check --only-binary :all: \
  --require-hashes -r "$tmp/requirements.txt"

log "git for the build users"
email=$(sudo -iu publisher git config --global --get user.email || true)
# git_for_builds <user> <home>
git_for_builds() {
  local user=$1 home=$2
  build_git() { sudo -u "$user" HOME="$home" git config --global "$@"; }
  build_git init.defaultBranch main
  build_git user.name >/dev/null || build_git user.name "gears-publisher"
  [[ -n "$email" ]] && build_git user.email "$email"
  # ci.yml's workaround: dylint (cargo gears lint --dylint) checks out rust-clippy's
  # `master`, which is now `main`. Serve its clone from a mirror where master is main.
  local mirror="$home/rust-clippy.git"
  if [[ ! -d "$mirror" ]]; then
    sudo -u "$user" git clone --bare --quiet https://github.com/rust-lang/rust-clippy "$mirror"
  else
    sudo -u "$user" git -C "$mirror" fetch --quiet origin '+refs/heads/*:refs/heads/*' || true
  fi
  sudo -u "$user" git -C "$mirror" branch -f master main
  sudo -u "$user" git -C "$mirror" symbolic-ref HEAD refs/heads/master
  build_git "url.file://$mirror.insteadOf" https://github.com/rust-lang/rust-clippy
}
git_for_builds publisher-build "$BUILD_HOME"
git_for_builds publisher-ci "$CI_HOME"

log "PostgreSQL"
systemctl enable --now postgresql
if [[ -f "$ETC/publisher.env" ]]; then
  DB_PASSWORD=$(sed -n 's|^DATABASE_URL=postgres://publisher:\([^@]*\)@.*|\1|p' "$ETC/publisher.env")
fi
DB_PASSWORD=${DB_PASSWORD:-$(openssl rand -hex 24)}
# SQL over stdin: the password never appears in a command line.
sudo -u postgres psql -qtAc "SELECT 1 FROM pg_roles WHERE rolname='publisher'" | grep -q 1 \
  || sudo -u postgres psql -qc "CREATE ROLE publisher LOGIN"
printf "ALTER ROLE publisher PASSWORD '%s';\n" "$DB_PASSWORD" | sudo -u postgres psql -q
sudo -u postgres psql -qtAc "SELECT 1 FROM pg_database WHERE datname='publisher'" | grep -q 1 \
  || sudo -u postgres createdb -O publisher publisher

log "service environment"
PUBLIC_IP=${PUBLIC_IP:-$(curl -fsS https://api.ipify.org || hostname -I | awk '{print $1}')}
DOMAIN="${PUBLIC_IP//./-}.sslip.io"
FIREBASE_PROJECT_ID=${FIREBASE_PROJECT_ID:-$(sed -n 's/^FIREBASE_PROJECT_ID=//p' "$ETC/publisher.env" 2>/dev/null || true)}
umask 027
sed -e "s|__DB_PASSWORD__|$DB_PASSWORD|" -e "s|__FIREBASE_PROJECT_ID__|${FIREBASE_PROJECT_ID:-set-me}|" \
  "$APP/src/deploy/publisher.env.template" > "$ETC/publisher.env"
umask 022
chown root:publisher "$ETC/publisher.env" && chmod 0640 "$ETC/publisher.env"

log "Caddy for $DOMAIN"
sed "s|__DOMAIN__|$DOMAIN|" "$APP/src/deploy/Caddyfile.template" > /etc/caddy/Caddyfile
systemctl enable caddy && systemctl reload-or-restart caddy

log "host hardening"
install -m 0644 "$APP/src/deploy/sshd-hardening.conf" /etc/ssh/sshd_config.d/00-gears-publisher.conf
sshd -t && systemctl reload ssh
systemctl disable --now rpcbind.socket rpcbind.service 2>/dev/null || true
# Names that do not exist must not resolve (see the file).
install -D -m 0644 "$APP/src/deploy/resolved-no-search-domain.conf" \
  /etc/systemd/resolved.conf.d/no-search-domain.conf
systemctl restart systemd-resolved

log "systemd unit"
install -m 0644 "$APP/src/deploy/gears-publisher.service" /etc/systemd/system/gears-publisher.service
systemctl daemon-reload
systemctl enable gears-publisher

cat <<EOF

Provisioned. Domain: https://$DOMAIN
Next:
  1. sudo -iu publisher gh auth login        (then: git config --global user.email <noreply>)
  2. deploy/deploy.sh app <ip> from the laptop   (builds and starts the service)
EOF
