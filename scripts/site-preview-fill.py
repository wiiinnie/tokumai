#!/usr/bin/env python3
"""Fill server/site/index.html's {{...}} placeholders with plausible values for a local
preview, the way a live scrai-faucet fills them from its download manifest.

Why not one blanket regex: {{SHA_*}} and {{META_*}} mean different things. Replacing them
all with the same string rendered the download cards as long runs of zeros (2026-09-04),
which hid the real layout instead of showing it.

Usage: site-preview-fill.py <index.html>
"""
import re
import sys

V = "0.4.6"

SHA = {
    "MACOS": "9f2c41ab7d6e05c38b1a4f97e2d0c6b58a3417ff9c02de71b48a6c35d09e1f7a",
    "WINDOWS": "3ad81e0c72f45b96d8e13c7a05f2b64e9c7d0a18f34b6e29c5017da8b3f2e64c",
    "DEB": "c47f9b2e18d0a635be714c8f92d3a0576e1bc4f8309da62b7e5c14098fa3d2b6",
    "APPIMAGE": "5e0b7c94a2f18d63e47c0951bda386f2c9e14b07d5a2f8b361c94e07da285fb3",
    "ANDROID": "81c3fa06d95e27b4c8013f7ae62d905bc4178e30fa9d62c5b71e04837fa16d29",
}
FILES = {
    "MACOS": "tokumai_%s_aarch64.dmg" % V,
    "WINDOWS": "tokumai_%s_x64-setup.exe" % V,
    "DEB": "tokumai_%s_amd64.deb" % V,
    "APPIMAGE": "tokumai_%s_amd64.AppImage" % V,
    "ANDROID": "tokumai_%s.apk" % V,
}
META = {
    "MACOS": "%s · 11.8 MB" % FILES["MACOS"],
    "WINDOWS": "%s · 9.3 MB" % FILES["WINDOWS"],
    "LINUX": "%s · 12.1 MB" % FILES["DEB"],
    "ANDROID": "%s · 24.6 MB" % FILES["ANDROID"],
}


def main(path: str) -> int:
    s = open(path).read()
    s = s.replace("{{VERSION_SHORT}}", V)
    s = s.replace("{{VERSION}}", "Testnet build %s" % V)
    s = s.replace("{{TESTNET}}", "1")
    for k, v in SHA.items():
        s = s.replace("{{SHA_%s}}" % k, v)
    for k, v in FILES.items():
        s = s.replace("{{FILE_%s}}" % k, v)
    for k, v in META.items():
        s = s.replace("{{META_%s}}" % k, v)
    # Download buttons: the SAME labels and primary/secondary split scrai-faucet renders,
    # or the preview shows two identical "Download" buttons on the Linux card and you end
    # up reviewing a layout the site never has.
    for k, label, primary in (
        ("MACOS", "Download .dmg", True),
        ("WINDOWS", "Download installer (.exe)", True),
        ("DEB", "Download .deb", True),
        ("APPIMAGE", "AppImage", False),
        ("ANDROID", "Download .apk", True),
    ):
        cls = "btn primary" if primary else "btn"
        s = s.replace("{{DL_%s}}" % k, '<a class="%s" href="#">%s</a>' % (cls, label))
    # No TestFlight link yet: an off button for the join link, and NOTHING for the optional
    # guide — same as the server, which renders an empty string when DL_IOS_GUIDE is unset.
    s = s.replace("{{DL_IOS}}", '<span class="btn off">Join on TestFlight · not published yet</span>')
    s = s.replace("{{DL_IOS_GUIDE}}", "")
    for k in ("MACOS", "WINDOWS", "LINUX", "ANDROID"):
        s = s.replace("{{CLS_%s}}" % k, " has")
    s = s.replace("{{CLS_IOS}}", " soon")
    s = s.replace(
        "{{NOTE_IOS_PENDING}}",
        '<div style="margin-top:8px">Apple Beta App Review pending — the join link '
        "appears here as soon as it is approved.</div>",
    )
    left = sorted(set(re.findall(r"\{\{[A-Z_]+\}\}", s)))
    if left:
        print("site-preview: unfilled placeholders -> %s" % ", ".join(left), file=sys.stderr)
        s = re.sub(r"\{\{[A-Z_]+\}\}", "—", s)
    open(path, "w").write(s)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
