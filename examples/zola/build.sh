#!/usr/bin/env sh
# No package.json, no Node: gqlfreez is a binary, Zola is a binary.
# Run from the site root: the pages are written under content/, outside data/.
set -e
gqlfreez . --endpoint https://countries.trevorblades.com/graphql
zola build
