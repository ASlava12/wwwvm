#!/usr/bin/env python3
"""Fetch public proxy lists into a local JSON file for the wwwvm web UI's proxy
dropdown (and, later, crates/proxy upstream chaining).

Python 3.6+, standard library only — meant to run from cron, e.g.:

    */30 * * * *  /path/to/wwwvm/scripts/fetch-proxies.py >> /tmp/wwwvm-proxies.log 2>&1

Each source is fetched independently and failures are skipped, so one dead
source never aborts the run. Output is a JSON object:

    {"updated": <unix-ts-or-0>, "proxies": [{"type","host","port","source"}, ...]}

`type` is one of http / https / socks4 / socks5. The list is deduped and capped.

NOTE: public proxies are UNTRUSTED third parties (they can log / MITM traffic) —
use them only as an upstream for non-sensitive traffic, never for credentials.
"""
import argparse
import json
import os
import re
import socket
import ssl
import sys
import time
import urllib.request
from urllib.error import URLError, HTTPError

# (name, url, kind) — kind tells the parser how to read the body:
#   "proto"  : lines like "socks5://1.2.3.4:1080" (protocol in the line)
#   "ipport" : lines like "1.2.3.4:8080" (protocol from `forced_type`)
#   "geonode": GeoNode JSON API ({"data":[{"ip","port","protocols":[...]}]})
SOURCES = [
    # Proxifly — protocol://ip:port, all protocols in one file.
    ("proxifly",
     "https://raw.githubusercontent.com/proxifly/free-proxy-list/main/proxies/all/data.txt",
     "proto", None),
    # TheSpeedX — one file per protocol, bare ip:port.
    ("thespeedx-http",
     "https://raw.githubusercontent.com/TheSpeedX/PROXY-List/master/http.txt",
     "ipport", "http"),
    ("thespeedx-socks4",
     "https://raw.githubusercontent.com/TheSpeedX/PROXY-List/master/socks4.txt",
     "ipport", "socks4"),
    ("thespeedx-socks5",
     "https://raw.githubusercontent.com/TheSpeedX/PROXY-List/master/socks5.txt",
     "ipport", "socks5"),
    # ProxyScrape API — bare ip:port per protocol.
    ("proxyscrape-http",
     "https://api.proxyscrape.com/v2/?request=displayproxies&protocol=http&timeout=10000&country=all",
     "ipport", "http"),
    ("proxyscrape-socks5",
     "https://api.proxyscrape.com/v2/?request=displayproxies&protocol=socks5&timeout=10000&country=all",
     "ipport", "socks5"),
    # GeoNode — JSON with per-entry protocol list.
    ("geonode",
     "https://proxylist.geonode.com/api/proxy-list?limit=500&page=1&sort_by=lastChecked&sort_type=desc",
     "geonode", None),
]

VALID_TYPES = {"http", "https", "socks4", "socks5"}
PROTO_RE = re.compile(r"^(https?|socks[45])://(\d{1,3}(?:\.\d{1,3}){3}):(\d{1,5})", re.I)
IPPORT_RE = re.compile(r"(\d{1,3}(?:\.\d{1,3}){3}):(\d{1,5})")


def valid_ip(ip):
    parts = ip.split(".")
    return len(parts) == 4 and all(p.isdigit() and 0 <= int(p) <= 255 for p in parts)


def valid_port(port):
    return port.isdigit() and 0 < int(port) <= 65535


def fetch(url, timeout):
    ctx = ssl.create_default_context()
    req = urllib.request.Request(url, headers={"User-Agent": "wwwvm-proxy-fetch/1"})
    with urllib.request.urlopen(req, timeout=timeout, context=ctx) as r:
        return r.read().decode("utf-8", "replace")


def parse_proto(body):
    out = []
    for line in body.splitlines():
        m = PROTO_RE.match(line.strip())
        if m and valid_ip(m.group(2)) and valid_port(m.group(3)):
            out.append((m.group(1).lower(), m.group(2), int(m.group(3))))
    return out


def parse_ipport(body, forced_type):
    out = []
    for line in body.splitlines():
        m = IPPORT_RE.search(line.strip())
        if m and valid_ip(m.group(1)) and valid_port(m.group(2)):
            out.append((forced_type, m.group(1), int(m.group(2))))
    return out


def parse_geonode(body):
    out = []
    try:
        data = json.loads(body).get("data", [])
    except (ValueError, AttributeError):
        return out
    for e in data:
        ip = str(e.get("ip", ""))
        port = str(e.get("port", ""))
        if not (valid_ip(ip) and valid_port(port)):
            continue
        for proto in e.get("protocols", []) or []:
            t = str(proto).lower()
            if t in VALID_TYPES:
                out.append((t, ip, int(port)))
    return out


def main():
    ap = argparse.ArgumentParser(description="Fetch public proxy lists -> JSON")
    here = os.path.dirname(os.path.abspath(__file__))
    ap.add_argument("--out", default=os.path.join(here, "..", "web", "proxies.json"))
    ap.add_argument("--limit", type=int, default=300, help="max proxies to keep")
    ap.add_argument("--timeout", type=int, default=15, help="per-request seconds")
    ap.add_argument("--types", default="http,https,socks4,socks5",
                    help="comma-separated subset of types to keep")
    args = ap.parse_args()

    want = {t.strip().lower() for t in args.types.split(",") if t.strip()}
    seen = set()
    proxies = []
    for name, url, kind, forced in SOURCES:
        try:
            body = fetch(url, args.timeout)
        except (URLError, HTTPError, socket.timeout, ssl.SSLError, OSError) as e:
            sys.stderr.write("[fetch-proxies] %s failed: %s\n" % (name, e))
            continue
        if kind == "proto":
            rows = parse_proto(body)
        elif kind == "ipport":
            rows = parse_ipport(body, forced)
        else:
            rows = parse_geonode(body)
        kept = 0
        for (t, ip, port) in rows:
            if t not in want:
                continue
            key = (t, ip, port)
            if key in seen:
                continue
            seen.add(key)
            proxies.append({"type": t, "host": ip, "port": port, "source": name})
            kept += 1
        sys.stderr.write("[fetch-proxies] %s: +%d (parsed %d)\n" % (name, kept, len(rows)))

    proxies = proxies[: args.limit]
    out = {"updated": int(time.time()), "proxies": proxies}
    out_path = os.path.abspath(args.out)
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    tmp = out_path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(out, f, separators=(",", ":"))  # compact — the browser fetches it
    os.replace(tmp, out_path)  # atomic
    sys.stderr.write("[fetch-proxies] wrote %d proxies -> %s\n" % (len(proxies), out_path))


if __name__ == "__main__":
    main()
