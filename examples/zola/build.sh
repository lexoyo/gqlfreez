#!/usr/bin/env sh
# No package.json, no Node: gqlfreez is a binary, Zola is a binary.
set -e
gqlfreez ./data --endpoint https://countries.trevorblades.com/graphql
zola build
