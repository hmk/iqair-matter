#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.10"
# dependencies = ["httpx[http2]"]
# ///
"""Standalone CLI for controlling an IQAir Atem X via IQAir's (undocumented) cloud API.

Protocol details adapted from https://github.com/ThioJoe/HA-IQAir-Integration.

Usage:
  IQAIR_EMAIL=... IQAIR_PASSWORD=... ./iqair.py login
  ./iqair.py devices               # list devices + raw state JSON
  ./iqair.py status                # state of the selected device
  ./iqair.py on | off
  ./iqair.py speed <1-8>
  ./iqair.py auto on|off
  ./iqair.py profile <1-3>         # smart mode profile: 1=quiet 2=balanced 3=power
  ./iqair.py light on|off
  ./iqair.py brightness <1-3>
  ./iqair.py lock on|off

Session tokens are cached in .iqair_session.json (mode 0600) next to this script.
"""
import base64
import json
import os
import re
import struct
import sys
from pathlib import Path

import httpx

SESSION_FILE = Path(__file__).with_name(".iqair_session.json")

DASHBOARD_URL = "https://dashboard.iqair.com/"
SIGNIN_URL = "https://website-api.airvisual.com/v2/auth/signin/by/email"
DEVICES_URL = "https://website-api.airvisual.com/v2/users/{user_id}/devices"
DEVICES_PARAMS = {"page": "1", "perPage": "15", "units.system": "imperial", "AQI": "US", "language": "en"}

GRPC_BASE = "https://cloud-api.iqair.io/"
GRPC_HEADERS = {
    "Content-Type": "application/grpc-web-text",
    "X-User-Agent": "grpc-web-javascript/0.1",
    "Accept": "application/grpc-web-text",
}

# Atem X uses the KLR service; serial numbers look like "KLR_xxxx".
SERVICES = {"KLR": "grpc.klr.v1.KLRService", "UI2": "grpc.ui2.v1.UI2Service"}

# protobuf tags: 0x10 = field 2 varint, 0x18 = field 3 varint
TAG_F2 = 0x10
TAG_F3 = 0x18


def die(msg: str) -> None:
    print(f"error: {msg}", file=sys.stderr)
    sys.exit(1)


def load_session() -> dict:
    if not SESSION_FILE.exists():
        die("no session; run `login` first")
    return json.loads(SESSION_FILE.read_text())


def save_session(data: dict) -> None:
    SESSION_FILE.write_text(json.dumps(data, indent=2))
    SESSION_FILE.chmod(0o600)


def login() -> None:
    email, password = os.environ.get("IQAIR_EMAIL"), os.environ.get("IQAIR_PASSWORD")
    if not email or not password:
        die("set IQAIR_EMAIL and IQAIR_PASSWORD")
    with httpx.Client(timeout=20) as c:
        r = c.post(SIGNIN_URL, json={"email": email, "password": password})
        if r.status_code >= 400:
            die(f"sign-in failed: HTTP {r.status_code} {r.text[:300]}")
        signin = r.json()

        # The gRPC bearer token is hardcoded in the dashboard's main JS bundle.
        html = c.get(DASHBOARD_URL).text
        m = re.search(r'src="/?(main\.[a-f0-9]+\.js)"', html)
        if not m:
            die("couldn't find dashboard main JS bundle")
        js = c.get(DASHBOARD_URL + m.group(1)).text
        m = re.search(r'cloudApiAuthToken:"Bearer ([^"]+)"', js)
        if not m:
            die("couldn't find cloudApiAuthToken in dashboard JS")

    session = {"user_id": signin["id"], "login_token": signin["loginToken"], "auth_token": m.group(1)}
    save_session(session)
    print(f"logged in as user {session['user_id']}")
    devices = get_devices(session)
    pick_device(session, devices)


def get_devices(session: dict) -> list[dict]:
    r = httpx.get(
        DEVICES_URL.format(user_id=session["user_id"]),
        params=DEVICES_PARAMS,
        headers={"x-login-token": session["login_token"]},
        timeout=20,
    )
    if r.status_code == 401:
        die("login token rejected (401); run `login` again")
    r.raise_for_status()
    return r.json()


def pick_device(session: dict, devices: list[dict]) -> None:
    purifiers = [d for d in devices if d.get("serialNumber")]
    for d in purifiers:
        print(f"  - {d.get('name')!r}  id={d.get('id')}  serial={d.get('serialNumber')}  model={d.get('model')}")
    atem = [d for d in purifiers if str(d.get("serialNumber", "")).upper().startswith("KLR")]
    chosen = (atem or purifiers or [None])[0]
    if not chosen:
        die("no devices with a serial number found on this account")
    prefix = chosen["serialNumber"].split("_")[0].upper()
    session.update(device_id=chosen["id"], serial=chosen["serialNumber"], prefix=prefix)
    save_session(session)
    print(f"selected {chosen.get('name')!r} ({chosen['serialNumber']}), service={SERVICES.get(prefix, '?')}")


def build_payload(serial: str, prefix: str, tag: int | None, value: int | None) -> str:
    sn = serial.replace(f"{prefix}_", "").lower().encode()
    body = bytearray([0x0A, len(sn)]) + sn  # field 1 (string): serial
    if value is not None:
        body += bytearray([tag, value])
    frame = bytearray([0x00]) + struct.pack(">I", len(body)) + body
    return base64.b64encode(frame).decode()


def decode_frames(text: str) -> list[tuple[int, bytes]]:
    frames = []
    for chunk in re.sub(r"(=+)([A-Za-z0-9+/])", r"\1\n\2", text).split("\n"):
        if not chunk:
            continue
        raw = base64.b64decode(chunk)
        if len(raw) >= 5:
            frames.append((raw[0], raw[5:]))
    return frames


def command(method: str, tag: int | None, value: int | None) -> None:
    s = load_session()
    if "serial" not in s:
        die("no device selected; run `login` or `devices`")
    service = SERVICES.get(s["prefix"])
    if not service:
        die(f"unknown device prefix {s['prefix']}")
    url = f"{GRPC_BASE}{service}/{method}"
    payload = build_payload(s["serial"], s["prefix"], tag, value)
    with httpx.Client(http2=True, timeout=20, headers={**GRPC_HEADERS, "Authorization": f"Bearer {s['auth_token']}"}) as c:
        r = c.post(url, content=payload)
    print(f"POST {url} -> HTTP {r.status_code}")
    grpc_status = r.headers.get("grpc-status")
    if grpc_status:
        print(f"  grpc-status={grpc_status} grpc-message={r.headers.get('grpc-message')}")
    for ftype, data in decode_frames(r.text):
        if ftype == 0x80:
            print(f"  trailers: {data.decode(errors='replace').strip()}")
        else:
            print(f"  data: {data.hex(' ') or '(empty)'}")


def on_off(arg: str) -> bool:
    if arg not in ("on", "off"):
        die("expected on|off")
    return arg == "on"


def int_arg(args: list[str], lo: int, hi: int) -> int:
    if not args or not args[0].isdigit() or not lo <= int(args[0]) <= hi:
        die(f"expected a number {lo}-{hi}")
    return int(args[0])


def main() -> None:
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(1)
    cmd, args = sys.argv[1], sys.argv[2:]

    if cmd == "login":
        login()
    elif cmd == "devices":
        s = load_session()
        devices = get_devices(s)
        print(json.dumps(devices, indent=2))
        pick_device(s, devices)
    elif cmd == "status":
        s = load_session()
        d = next((d for d in get_devices(s) if d.get("id") == s.get("device_id")), None)
        print(json.dumps(d, indent=2) if d else "selected device not found")
    elif cmd in ("on", "off"):
        # klr.v1.PowerMode: OFF=1, ON=2, STANDBY=3
        command("SetPowerMode", TAG_F2, 2 if cmd == "on" else 3)
    elif cmd == "speed":
        command("SetFanSpeed", TAG_F3, int_arg(args, 1, 8))
    elif cmd == "auto":
        command("SetAutoMode", TAG_F2, 1 if on_off(args[0] if args else "") else None)
    elif cmd == "profile":
        command("SetAutoModeProfile", TAG_F2, int_arg(args, 1, 3))
    elif cmd == "light":
        command("SetLightIndicator", TAG_F2, 1 if on_off(args[0] if args else "") else None)
    elif cmd == "brightness":
        command("SetLightLevel", TAG_F2, int_arg(args, 1, 3))
    elif cmd == "lock":
        command("SetDefaultLocks", TAG_F2, 1 if on_off(args[0] if args else "") else None)
    else:
        die(f"unknown command {cmd!r}")


if __name__ == "__main__":
    main()
