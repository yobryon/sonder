#!/usr/bin/env bash
# Finalise the service worker in a built site directory: replace `__BUILD__` in sw.js with a
# hash of every shipped file and `/*__PRECACHE__*/[]` with the list of files to precache.
#
#   scripts/sw-inject.sh site        # called by build-site.sh; safe to re-run
#
# The hash changes whenever any file does, so a new deploy gets a new cache name and the old
# one is dropped on activate (see crates/sonder-web/static/sw.js).
set -euo pipefail
OUT="${1:?usage: sw-inject.sh <site-dir>}"
[ -f "$OUT/sw.js" ] || { echo "sw-inject: no $OUT/sw.js" >&2; exit 1; }

# Restore the template first so re-runs are idempotent.
cp "$(dirname "$0")/../crates/sonder-web/static/sw.js" "$OUT/sw.js"

python3 - "$OUT" <<'EOF'
import hashlib, json, os, sys
out = sys.argv[1]
files = []
for root, dirs, names in os.walk(out):
    rel_root = os.path.relpath(root, out)
    if rel_root.split(os.sep)[0] == 'records':
        dirs[:] = []
        continue
    for n in names:
        rel = os.path.normpath(os.path.join(rel_root, n)).replace(os.sep, '/')
        if rel in ('sw.js', '.nojekyll'):
            continue
        files.append(rel)
files.sort()
h = hashlib.sha256()
for rel in files:
    h.update(rel.encode())
    with open(os.path.join(out, rel), 'rb') as f:
        h.update(f.read())
build = h.hexdigest()[:12]
sw = os.path.join(out, 'sw.js')
src = open(sw).read()
assert "'__BUILD__'" in src and '/*__PRECACHE__*/[]' in src, 'sw.js template markers missing'
src = src.replace("'__BUILD__'", json.dumps(build)).replace('/*__PRECACHE__*/[]', json.dumps(['./' + f for f in files]))
open(sw, 'w').write(src)
print(f"sw.js: build {build}, {len(files)} files precached")
EOF
