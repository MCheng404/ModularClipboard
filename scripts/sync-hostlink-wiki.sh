#!/usr/bin/env bash
# Mirror the Host Link documents from docs/ into the GitHub Wiki.
#
# One-time prerequisite: open https://github.com/MCheng404/ModularClipboard/wiki
# in a browser and create the first page. GitHub does not create the .wiki.git
# repository (and rejects pushes to it) until that page exists.
#
# Usage: ./scripts/sync-hostlink-wiki.sh
# Requires: gh (authenticated), git, and the local proxy on 127.0.0.1:7897.

set -euo pipefail

REPO="MCheng404/ModularClipboard"
WIKI_REPO="https://github.com/${REPO}.wiki.git"
PROXY="http://127.0.0.1:7897"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

git_cfg=(-c "http.proxy=${PROXY}" -c "https.proxy=${PROXY}")

TOKEN="$(gh auth token)" || {
  echo "gh is not authenticated; run 'gh auth login' first" >&2
  exit 1
}
AUTH_URL="https://x-access-token:${TOKEN}@github.com/${REPO}.wiki.git"

echo "==> checking wiki repository"
if ! git "${git_cfg[@]}" ls-remote "${AUTH_URL}" >/dev/null 2>&1; then
  echo "Wiki repository is not initialised yet." >&2
  echo "Open https://github.com/${REPO}/wiki and create the first page, then re-run." >&2
  exit 1
fi

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

echo "==> cloning wiki"
git "${git_cfg[@]}" clone --depth 1 "${AUTH_URL}" "${WORK}/wiki"

cp "${ROOT}/docs/HostLink-Protocol.md" "${WORK}/wiki/HostLink-Protocol.md"
cp "${ROOT}/docs/Dock-Integration-Guide.md" "${WORK}/wiki/Dock-Integration-Guide.md"

if [[ ! -f "${WORK}/wiki/Home.md" ]]; then
  cat > "${WORK}/wiki/Home.md" <<'EOF'
# ModularClipboard Wiki

## Integration

- [Host Link Protocol v1](HostLink-Protocol)
- [Dock Integration Guide](Dock-Integration-Guide)

Canonical copies live in `docs/` of the main repository.
EOF
fi

cd "${WORK}/wiki"
git "${git_cfg[@]}" add -A
if git diff --cached --quiet; then
  echo "==> wiki is already up to date"
  exit 0
fi
git -c user.name="MCheng404" -c user.email="MCheng404@users.noreply.github.com" \
  commit -m "docs: sync Host Link protocol and Dock integration guide"
echo "==> pushing wiki"
git "${git_cfg[@]}" push origin HEAD
echo "==> done: https://github.com/${REPO}/wiki"
