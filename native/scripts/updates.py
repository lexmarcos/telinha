#!/usr/bin/env python3
"""Prepares dist/download/ for the server's /download folder, after package.sh:

  <version>/        the packages
  latest.json       the newest version: per package, its file, size and SHA-256
  latest.json.sig   Ed25519 signature of latest.json (what the app checks)
  versions.json     the last versions, for the download page

Usage: updates.py SIGNING_KEY.pem [URL of the current versions.json]
The key is an Ed25519 private key in PEM (openssl genpkey -algorithm ed25519).
Its public half goes into the app as TELINHA_UPDATE_KEY:
  openssl pkey -in key.pem -pubout -outform DER | tail -c 32 | base64
"""
import base64, datetime, hashlib, json, os, re, shutil, subprocess, sys, urllib.request

KEEP = 3
root = os.path.join(os.path.dirname(__file__), "..")
dist = os.path.join(root, "dist")
version = re.search(r'^version = "(.+)"', open(os.path.join(root, "Cargo.toml")).read(), re.M).group(1)
key = sys.argv[1]
packages = {
    "appimage": f"Telinha-{version}-x86_64.AppImage",
    "deb": f"telinha_{version}_amd64.deb",
    "windows": f"Telinha-{version}-portatil.zip",
    "installer": f"Telinha-{version}-instalador.exe",
}

out = os.path.join(dist, "download")
shutil.rmtree(out, ignore_errors=True)
os.makedirs(os.path.join(out, version))
files = {}
for kind, name in packages.items():
    src = os.path.join(dist, name)
    dst = os.path.join(out, version, name)
    shutil.copyfile(src, dst)
    files[kind] = {
        "file": f"{version}/{name}",
        "size": os.path.getsize(dst),
        "sha256": hashlib.sha256(open(dst, "rb").read()).hexdigest(),
    }
release = {"version": version, "date": datetime.date.today().isoformat(), "files": files}

latest = os.path.join(out, "latest.json")
with open(latest, "w") as f:
    json.dump(release, f, indent=1)
sig = subprocess.run(["openssl", "pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", latest], capture_output=True, check=True).stdout
with open(latest + ".sig", "w") as f:
    f.write(base64.b64encode(sig).decode() + "\n")

previous = []
if len(sys.argv) > 2:
    try:
        previous = json.load(urllib.request.urlopen(sys.argv[2], timeout=20))["versions"]
    except Exception as e:  # first release, or the server is down
        print(f"no previous versions.json ({e})")
versions = [release] + [v for v in previous if v["version"] != version]
with open(os.path.join(out, "versions.json"), "w") as f:
    json.dump({"versions": versions[:KEEP]}, f, indent=1)
print("\n".join(v["version"] for v in versions[:KEEP]))
