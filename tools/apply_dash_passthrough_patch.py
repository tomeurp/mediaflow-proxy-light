#!/usr/bin/env python3
"""Apply DASH passthrough endpoint patch to MediaFlow Proxy Light.

Run from the repository root after copying this package's src/mpd/dash_passthrough.rs
into src/mpd/dash_passthrough.rs, or run this script from the package root with
REPO_ROOT pointing at your checkout.
"""
from pathlib import Path
import os
import shutil

pkg_root = Path(__file__).resolve().parents[1]
repo = Path(os.environ.get("REPO_ROOT", ".")).resolve()

src_file = pkg_root / "src" / "mpd" / "dash_passthrough.rs"
dst_file = repo / "src" / "mpd" / "dash_passthrough.rs"
dst_file.parent.mkdir(parents=True, exist_ok=True)
shutil.copy2(src_file, dst_file)
print(f"[OK] wrote {dst_file}")

mod_rs = repo / "src" / "mpd" / "mod.rs"
s = mod_rs.read_text()
if "pub mod dash_passthrough;" not in s:
    if "pub mod handler;" in s:
        s = s.replace("pub mod handler;", "pub mod dash_passthrough; pub mod handler;", 1)
    else:
        s = "pub mod dash_passthrough;\n" + s
    mod_rs.write_text(s)
    print(f"[OK] patched {mod_rs}")
else:
    print(f"[SKIP] {mod_rs} already patched")

main_rs = repo / "src" / "main.rs"
s = main_rs.read_text()

# Add import for the handler near the MPD handler use block. Works with both formatted
# and compact one-line main.rs.
if "mpd::dash_passthrough::mpd_dash_passthrough_handler" not in s:
    marker = "use mpd::handler::{"
    idx = s.find(marker)
    if idx != -1:
        s = s[:idx] + "use mpd::dash_passthrough::mpd_dash_passthrough_handler; " + s[idx:]
    else:
        raise SystemExit("Could not find MPD handler use block in src/main.rs")
else:
    print("[SKIP] main.rs import already present")

route_snippet = (
    '.route("/dash.mpd", web::get().to(mpd_dash_passthrough_handler)) '
    '.route("/dash.mpd", web::head().to(mpd_dash_passthrough_handler)) '
    '.route("/manifest.dash.mpd", web::get().to(mpd_dash_passthrough_handler)) '
    '.route("/manifest.dash.mpd", web::head().to(mpd_dash_passthrough_handler)) '
)

if '/dash.mpd' not in s:
    # Insert after the /proxy/mpd scope opens, before existing /manifest routes.
    needle = 'web::scope("/proxy/mpd")'
    pos = s.find(needle)
    if pos == -1:
        raise SystemExit("Could not find web::scope(\"/proxy/mpd\") in src/main.rs")
    insert_pos = pos + len(needle)
    s = s[:insert_pos] + " " + route_snippet + s[insert_pos:]
    print("[OK] patched MPD dash passthrough routes")
else:
    print("[SKIP] main.rs routes already present")

main_rs.write_text(s)
print(f"[OK] wrote {main_rs}")
