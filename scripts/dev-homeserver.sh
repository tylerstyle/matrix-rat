#!/usr/bin/env bash
# Runs a throwaway local Matrix homeserver (continuwuity) for testing, on
# http://127.0.0.1:6167 with server name "localhost". Data lives in target/dev-homeserver
# and can be deleted at any time. Registration needs the token "devtoken".
set -euo pipefail
cd "$(dirname "$0")/.."
dir="$PWD/target/dev-homeserver"
mkdir -p "$dir/db"
cat > "$dir/config.toml" <<CONF
[global]
server_name = "localhost"
database_path = "$dir/db"
address = "127.0.0.1"
port = 6167
allow_registration = true
registration_token = "devtoken"
allow_federation = false
trusted_servers = []
new_user_displayname_suffix = ""
log = "warn"
CONF
exec nix-shell -p matrix-continuwuity --run "conduwuit -c '$dir/config.toml'"
