#!/usr/bin/env bash
# Tidies the accounts of the local test homeserver (scripts/dev-homeserver.sh) before a
# run of the server-backed tests: every test logs in as a new device and several create
# rooms, and neither goes away by itself. Hundreds of stale devices make every encrypted
# message slow to send, and the tests begin to time out.
#
# Removes all devices of the test users except the one this script logs in with (which
# it logs out at the end), and leaves and forgets every room that is not the shared
# "Test Lounge". Only ever point this at the throwaway test server.
set -euo pipefail
hs=${MATRIX_RAT_TEST_HOMESERVER:-http://127.0.0.1:6167}
case "$hs" in
  http://127.0.0.1:*|http://localhost:*) ;;
  *) echo "refusing to clean $hs: not a local test server" >&2; exit 1 ;;
esac
python3 - "$hs" <<'PY'
import json, sys, urllib.request, urllib.error

hs = sys.argv[1]

def call(method, path, token=None, body=None):
    request = urllib.request.Request(hs + path, method=method,
        data=None if body is None else json.dumps(body).encode())
    request.add_header("Content-Type", "application/json")
    if token:
        request.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(request, timeout=60) as answer:
            return answer.status, json.load(answer)
    except urllib.error.HTTPError as err:
        try:
            return err.code, json.load(err)
        except Exception:
            return err.code, {}

for user in ("alice", "bob"):
    password = user + "pass"
    ident = {"type": "m.id.user", "user": user}
    status, login = call("POST", "/_matrix/client/v3/login",
        body={"type": "m.login.password", "identifier": ident, "password": password,
              "initial_device_display_name": "test server cleanup"})
    if status != 200:
        print(f"{user}: could not log in ({status})")
        continue
    token, own = login["access_token"], login["device_id"]

    _, listed = call("GET", "/_matrix/client/v3/devices", token)
    stale = [d["device_id"] for d in listed.get("devices", []) if d["device_id"] != own]
    removed = 0
    for at in range(0, len(stale), 100):
        batch = stale[at:at + 100]
        status, answer = call("POST", "/_matrix/client/v3/delete_devices", token,
            {"devices": batch})
        if status == 401 and "session" in answer:
            status, answer = call("POST", "/_matrix/client/v3/delete_devices", token,
                {"devices": batch, "auth": {"type": "m.login.password",
                 "identifier": ident, "password": password,
                 "session": answer["session"]}})
        if status == 200:
            removed += len(batch)

    _, joined = call("GET", "/_matrix/client/v3/joined_rooms", token)
    left = 0
    for room in joined.get("joined_rooms", []):
        _, name = call("GET", f"/_matrix/client/v3/rooms/{room}/state/m.room.name", token)
        if name.get("name") == "Test Lounge":
            continue
        call("POST", f"/_matrix/client/v3/rooms/{room}/leave", token, {})
        call("POST", f"/_matrix/client/v3/rooms/{room}/forget", token, {})
        left += 1

    call("POST", "/_matrix/client/v3/logout", token, {})
    print(f"{user}: removed {removed} of {len(stale)} stale devices, left {left} rooms")
PY
