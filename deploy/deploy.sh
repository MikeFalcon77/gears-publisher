#!/usr/bin/env bash
# Laptop-side deployment over SSH (root@<ip>).
#
#   deploy/deploy.sh provision <ip>            sync sources, install and configure everything
#                                              (idempotent: run it again after any change)
#   deploy/deploy.sh secrets <ip> <env-file>   copy CARGO_REGISTRY_TOKEN (and the optional
#                                              read-only GITHUB_CI_TOKEN for PR CI) from a local .env
#   deploy/deploy.sh config <ip> [file]        install the app config (default: config/publisher.local.yaml
#                                              if it exists, else config/publisher.yaml); replaces the admin list
#   deploy/deploy.sh app <ip>                  sync, build UI and server on the host, restart
#   deploy/deploy.sh status <ip>               service state and recent log
#
# provision reads FIREBASE_PROJECT_ID, GIT_EMAIL (the email of the GitHub account that
# opens release PRs) and CARGO_JOBS (parallel build jobs) from the environment; each
# is kept from the previous run when not set.
# The UI build needs the public Firebase web config: VITE_FIREBASE_API_KEY,
# VITE_FIREBASE_AUTH_DOMAIN, VITE_FIREBASE_PROJECT_ID in the environment or in web/.env.local.
set -euo pipefail
cd "$(dirname "$0")/.."

cmd=${1:-}; ip=${2:-}
[[ -n "$cmd" && -n "$ip" ]] || { sed -n '2,17p' "$0"; exit 2; }
host="root@$ip"
APP=/opt/gears-publisher
KEY=${PUBLISHER_SSH_KEY:-$HOME/.ssh/id_ed25519_gears_publisher}
SSH_OPTS=(-o StrictHostKeyChecking=accept-new)
[[ -f "$KEY" ]] && SSH_OPTS+=(-i "$KEY" -o IdentitiesOnly=yes)
ssh_() { ssh "${SSH_OPTS[@]}" "$host" "$@"; }

sync() {
  ssh_ "install -d $APP/src"
  rsync -az --delete -e "ssh ${SSH_OPTS[*]}" \
    --exclude target --exclude .git --exclude .local --exclude node_modules \
    --exclude web/dist --exclude '.env*' --exclude 'web/.env*' \
    ./ "$host:$APP/src/"
  ssh_ "chown -R publisher:publisher $APP/src 2>/dev/null || true"
}

firebase_env() {
  local file=web/.env.local
  for key in VITE_FIREBASE_API_KEY VITE_FIREBASE_AUTH_DOMAIN VITE_FIREBASE_PROJECT_ID; do
    local value=${!key:-}
    [[ -z "$value" && -f "$file" ]] && value=$(sed -n "s/^$key=//p" "$file" | tr -d '"')
    [[ -n "$value" ]] || { echo "missing $key (environment or $file)" >&2; exit 1; }
    printf '%s=%q ' "$key" "$value"
  done
}

case "$cmd" in
  provision)
    sync
    ssh_ "FIREBASE_PROJECT_ID=$(printf %q "${FIREBASE_PROJECT_ID:-}") GIT_EMAIL=$(printf %q "${GIT_EMAIL:-}") \
          CARGO_JOBS=$(printf %q "${CARGO_JOBS:-}") PUBLIC_IP='$ip' bash $APP/src/deploy/provision.sh"
    ;;
  secrets)
    file=${3:?env file with CARGO_REGISTRY_TOKEN}
    # Via stdin: the token never appears in a command line.
    grep -E '^(export )?(CARGO_REGISTRY_TOKEN|GITHUB_CI_TOKEN)=' "$file" | sed 's/^export //' \
      | ssh_ "install -o root -g publisher -m 0640 /dev/stdin /etc/gears-publisher/secrets.env"
    echo "secrets.env updated"
    ;;
  config)
    file=${3:-config/publisher.local.yaml}
    [[ -n "${3:-}" || -f "$file" ]] || file=config/publisher.yaml
    echo "installing $file"
    scp -q "${SSH_OPTS[@]}" "$file" "$host:/etc/gears-publisher/publisher.yaml"
    ssh_ "chown root:publisher /etc/gears-publisher/publisher.yaml && chmod 0640 /etc/gears-publisher/publisher.yaml && systemctl restart gears-publisher"
    echo "config installed, service restarted"
    ;;
  app)
    vite=$(firebase_env)
    sync
    # The script goes over stdin: `sudo -i` would join a multi-line `-c` into one line.
    # Built as publisher; installed root-owned, so the service cannot change its own code.
    ssh_ "sudo -iu publisher bash -euo pipefail -s" <<SCRIPT
export PATH=\$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin
cd $APP/src/web
npm ci --no-audit --no-fund
$vite npm run build
cd $APP/src
cargo build --release --locked -p publisher-server
SCRIPT
    ssh_ "set -e
          install -o root -g root -m 0755 $APP/src/target/release/publisher-server $APP/bin/publisher-server.new
          mv $APP/bin/publisher-server.new $APP/bin/publisher-server
          rm -rf $APP/web.new && mkdir -p $APP/web.new && cp -r $APP/src/web/dist $APP/web.new/dist
          chown -R root:root $APP/web.new && rm -rf $APP/web && mv $APP/web.new $APP/web
          [ -f /etc/gears-publisher/publisher.yaml ] || install -o root -g publisher -m 0640 $APP/src/config/publisher.yaml /etc/gears-publisher/publisher.yaml
          systemctl restart gears-publisher && sleep 3 && systemctl --no-pager --lines=0 status gears-publisher"
    ;;
  status)
    ssh_ "systemctl --no-pager status gears-publisher caddy | head -20; journalctl -u gears-publisher -n 40 --no-pager"
    ;;
  *)
    sed -n '2,17p' "$0"; exit 2
    ;;
esac
