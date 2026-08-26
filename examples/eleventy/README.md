# gqlfreez + Eleventy

```
src/_data/api.graphql   ← you write this
src/_data/api.json      ← gqlfreez writes this
```

That is the whole integration. No plugin, no `_data/*.js` wrapper, no fetch caching —
Eleventy already loads any `.json` in `_data/` as a
[global](https://www.11ty.dev/docs/data-global/), so `api.countries` is available in
every template.

```bash
# gqlfreez is not on npm yet, so grab the binary first
curl -fsSL https://github.com/lexoyo/gqlfreez/releases/latest/download/gqlfreez-linux-x64 \
  -o /usr/local/bin/gqlfreez && chmod +x /usr/local/bin/gqlfreez

npm install
npm run build     # `prebuild` runs gqlfreez, then Eleventy builds
```
