#!/usr/bin/env sh
# No package.json, no Node. Grab the binary and run it.
set -e

if ! command -v gqlfreez > /dev/null 2>&1; then
  echo "gqlfreez not found. Grab a binary from the releases page:"
  echo
  echo "  curl -fsSL https://github.com/lexoyo/gqlfreez/releases/latest/download/gqlfreez-linux-x64 \\"
  echo "    -o /usr/local/bin/gqlfreez && chmod +x /usr/local/bin/gqlfreez"
  echo
  exit 1
fi

gqlfreez ./data --endpoint https://countries.trevorblades.com/graphql
zola build
