set dotenv-load

root := justfile_directory()
data := root / "data"
image := env("IMAGE", "iqair-matter:dev")
web := env("WEB_BIND", "127.0.0.1:8080")
cli := root / "iqair.py"

# list recipes
default:
    @just --list --unsorted

# ── bridge (Rust service) ──────────────────────────────────────────────

# run the bridge locally; state in ./data, web UI on WEB_BIND (default 127.0.0.1:8080)
[group('bridge')]
run *args:
    DATA_DIR={{data}} WEB_BIND={{web}} cargo run {{args}}

# reuse the Python CLI's session so the bridge starts signed in (no password needed)
[group('bridge')]
import-session:
    mkdir -p {{data}}
    install -m 600 {{root}}/.iqair_session.json {{data}}/session.json

# show the running bridge's state
[group('bridge')]
state:
    curl -fsS http://{{web}}/api/state | python3 -m json.tool

# send a control to the running bridge, e.g. `just send speed 3`, `just send power false`
[group('bridge')]
send action value:
    curl -fsS -X POST -H 'Content-Type: application/json' \
      -d '{"action":"{{action}}","value":{{value}}}' http://{{web}}/api/control \
      | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("error") or "ok")'

# delete local bridge state (credentials, session, Matter pairing)
[group('bridge')]
reset:
    rm -rf {{data}}

# ── checks ─────────────────────────────────────────────────────────────

# unit tests
[group('dev')]
test:
    cargo test

# formatting + clippy
[group('dev')]
check:
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

# format the code
[group('dev')]
fmt:
    cargo fmt

# ── container ──────────────────────────────────────────────────────────

# build the container image
[group('container')]
image:
    docker build -t {{image}} {{root}}

# run the image locally (bridge network: web UI works, Matter pairing won't)
[group('container')]
container: image
    mkdir -p {{data}}
    docker run --rm -it --name iqair-matter \
      -p 127.0.0.1:8080:8080 -v {{data}}:/data \
      --read-only --cap-drop ALL \
      -e IQAIR_EMAIL -e IQAIR_PASSWORD -e WEB_PASSWORD \
      {{image}}

# memory + CPU of the running container
[group('container')]
stats:
    docker stats --no-stream iqair-matter

# ── cloud CLI (iqair.py, talks to IQAir directly) ──────────────────────

# sign in (prompts for anything not set in IQAIR_EMAIL / IQAIR_PASSWORD)
[group('cli')]
login:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "${IQAIR_EMAIL:-}" ]; then read -rp "IQAir email: " IQAIR_EMAIL; fi
    if [ -z "${IQAIR_PASSWORD:-}" ]; then read -rsp "IQAir password: " IQAIR_PASSWORD; echo; fi
    IQAIR_EMAIL="$IQAIR_EMAIL" IQAIR_PASSWORD="$IQAIR_PASSWORD" {{cli}} login

# list devices on the account (raw JSON) and reselect the Atem X
[group('cli')]
devices:
    {{cli}} devices

# full state of the selected device
[group('cli')]
status:
    {{cli}} status

# power on
[group('cli')]
on:
    {{cli}} on

# power off (standby)
[group('cli')]
off:
    {{cli}} off

# fan speed level 1-8
[group('cli')]
speed level:
    {{cli}} speed {{level}}

# smart/auto mode: on|off
[group('cli')]
auto state:
    {{cli}} auto {{state}}

# smart mode profile: 1=quiet 2=balanced 3=power
[group('cli')]
profile n:
    {{cli}} profile {{n}}

# indicator light: on|off
[group('cli')]
light state:
    {{cli}} light {{state}}

# light brightness 1-3
[group('cli')]
brightness level:
    {{cli}} brightness {{level}}

# control panel lock: on|off
[group('cli')]
lock state:
    {{cli}} lock {{state}}

# forget the CLI's cached session tokens
[group('cli')]
logout:
    rm -f {{root}}/.iqair_session.json
