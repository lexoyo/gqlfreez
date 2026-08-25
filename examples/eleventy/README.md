# gqlfreez + Eleventy

```bash
npm install
npm run build
```

`prebuild` runs `gqlfreez` before Eleventy, so `src/queries/countries.graphql` becomes
`src/queries/countries.json`. `src/_data/countries.js` imports that file — Eleventy never
touches the network.

The interesting part is what is **not** here: no Eleventy plugin, no async data function,
no fetch caching. Swap Eleventy for Astro, Hugo or a shell script and the query file does
not change.
