#!/usr/bin/env python3
"""asc.py — ask App Store Connect about the app, without a browser.

`xcrun altool --list-builds` was removed from Xcode, so after an upload there is no way
to see whether Apple has finished processing a build short of reloading the web UI. This
mints the same ES256 token altool uses (ASC_ADMIN_KEY_ID / ASC_ISSUER_ID from .env, key
at ~/.appstoreconnect/private_keys/AuthKey_<id>.p8) and calls the REST API.

    scripts/asc.py builds [n]   recent builds: version (build) state  age
    scripts/asc.py versions     App Store version records and their state
    scripts/asc.py get <path>   any endpoint, raw JSON (e.g. v1/apps)

PyJWT is not installed anywhere in this project; the token is 30 lines of `cryptography`.
"""
import base64, json, os, sys, time, urllib.request, urllib.error, datetime, re
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec, utils as asym_utils

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUNDLE_ID = "com.tokumai.app"

def env(name):
    v = os.environ.get(name)
    if v:
        return v
    with open(os.path.join(ROOT, ".env")) as fh:
        for line in fh:
            m = re.match(rf"^{name}=(.*)$", line.strip())
            if m:
                return m.group(1).strip().strip('"\'')
    sys.exit(f"{name} is not set (.env)")

def token():
    kid, iss = env("ASC_ADMIN_KEY_ID"), env("ASC_ISSUER_ID")
    path = os.path.expanduser(f"~/.appstoreconnect/private_keys/AuthKey_{kid}.p8")
    with open(path, "rb") as fh:
        key = serialization.load_pem_private_key(fh.read(), password=None)
    b64 = lambda d: base64.urlsafe_b64encode(d).rstrip(b"=")
    head = b64(json.dumps({"alg": "ES256", "kid": kid, "typ": "JWT"}).encode())
    body = b64(json.dumps({"iss": iss, "exp": int(time.time()) + 1200, "aud": "appstoreconnect-v1"}).encode())
    der = key.sign(head + b"." + body, ec.ECDSA(hashes.SHA256()))
    r, s = asym_utils.decode_dss_signature(der)           # the API wants raw r||s, not DER
    sig = b64(r.to_bytes(32, "big") + s.to_bytes(32, "big"))
    return (head + b"." + body + b"." + sig).decode()

def api(path):
    req = urllib.request.Request(f"https://api.appstoreconnect.apple.com/{path.lstrip('/')}",
                                 headers={"Authorization": f"Bearer {token()}"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.load(r)
    except urllib.error.HTTPError as e:
        sys.exit(f"HTTP {e.code}: {e.read().decode()[:400]}")

def app_id():
    d = api(f"v1/apps?filter[bundleId]={BUNDLE_ID}")["data"]
    if not d:
        sys.exit(f"no app with bundle id {BUNDLE_ID}")
    return d[0]["id"]

def ago(stamp):
    t = datetime.datetime.fromisoformat(stamp.replace("Z", "+00:00"))
    m = int((datetime.datetime.now(datetime.timezone.utc) - t).total_seconds() // 60)
    return f"{m} min ago" if m < 90 else f"{m // 60} h ago"

cmd = sys.argv[1] if len(sys.argv) > 1 else "builds"
if cmd == "builds":
    n = sys.argv[2] if len(sys.argv) > 2 else "5"
    r = api(f"v1/builds?filter[app]={app_id()}&sort=-uploadedDate&limit={n}&include=preReleaseVersion")
    vers = {i["id"]: i["attributes"]["version"] for i in r.get("included", []) if i["type"] == "preReleaseVersions"}
    for b in r["data"]:
        a = b["attributes"]
        pre = b["relationships"]["preReleaseVersion"]["data"]
        print(f"{vers.get(pre and pre['id'], '?'):>8} ({a['version']})  {a['processingState']:<10} "
              f"{'expired' if a.get('expired') else '':<8} {ago(a['uploadedDate'])}")
elif cmd == "versions":
    r = api(f"v1/apps/{app_id()}/appStoreVersions?limit=5")
    for v in r["data"]:
        a = v["attributes"]
        print(f"{a['versionString']:>8}  {a['appStoreState']}  ({a['platform']}, {ago(a['createdDate'])})")
elif cmd == "get":
    print(json.dumps(api(sys.argv[2]), indent=2))
else:
    sys.exit(__doc__)
