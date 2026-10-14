#!/usr/bin/env bash
# Parses and lints the page's scripts. index.html embeds its JavaScript inline,
# so the blocks are pulled out to files first — neither node nor eslint can read
# a whole HTML document. Everything is assembled in a temporary directory so the
# repository stays free of a JavaScript toolchain.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."

page=static/index.html
worker=static/sw.js
capture=.github/screenshots/capture.mjs
compare=.github/screenshots/compare.mjs

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

awk -v outdir="$work" '
  /^[[:space:]]*<script>[[:space:]]*$/ { n++; inside=1; next }
  /^[[:space:]]*<\/script>[[:space:]]*$/ { inside=0; next }
  inside { print > (outdir "/inline-" n ".js") }
' "$page"

shopt -s nullglob
scripts=("$work"/inline-*.js)
if [ ${#scripts[@]} -eq 0 ]; then
  echo "found no inline scripts in $page — the extraction pattern has gone stale" >&2
  exit 1
fi

cp "$worker" "$work/sw.js"
cp "$capture" "$work/capture.mjs"
cp "$compare" "$work/compare.mjs"
scripts+=("$work/sw.js" "$work/capture.mjs" "$work/compare.mjs")

for f in "${scripts[@]}"; do
  node --check "$f"
done

cp .github/eslint.config.mjs "$work/"
npm install --prefix "$work" --silent --no-save --no-audit --no-fund eslint@9 globals@16
# Run from the work directory so eslint finds its config and treats the
# extracted files as the project it is linting.
(cd "$work" && ./node_modules/.bin/eslint -- *.js *.mjs)

echo "checked ${#scripts[@]} script(s) from $page, $worker, $capture and $compare"
