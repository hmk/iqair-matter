iqair := justfile_directory() / "iqair.py"

# list recipes
default:
    @just --list

# sign in (prompts for anything not set in IQAIR_EMAIL / IQAIR_PASSWORD)
login:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "${IQAIR_EMAIL:-}" ]; then read -rp "IQAir email: " IQAIR_EMAIL; fi
    if [ -z "${IQAIR_PASSWORD:-}" ]; then read -rsp "IQAir password: " IQAIR_PASSWORD; echo; fi
    IQAIR_EMAIL="$IQAIR_EMAIL" IQAIR_PASSWORD="$IQAIR_PASSWORD" {{iqair}} login

# list devices on the account (raw JSON) and reselect the Atem X
devices:
    {{iqair}} devices

# full state of the selected device
status:
    {{iqair}} status

# power on
on:
    {{iqair}} on

# power off (standby)
off:
    {{iqair}} off

# fan speed level 1-8
speed level:
    {{iqair}} speed {{level}}

# smart/auto mode: on|off
auto state:
    {{iqair}} auto {{state}}

# smart mode profile: 1=quiet 2=balanced 3=power
profile n:
    {{iqair}} profile {{n}}

# indicator light: on|off
light state:
    {{iqair}} light {{state}}

# light brightness 1-3
brightness level:
    {{iqair}} brightness {{level}}

# control panel lock: on|off
lock state:
    {{iqair}} lock {{state}}

# forget the cached session tokens
logout:
    rm -f {{justfile_directory()}}/.iqair_session.json
