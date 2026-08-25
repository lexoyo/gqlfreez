# gqlfreez + Eleventy

```bash
# gqlfreez is not on npm yet, so grab the binary first
curl -fsSL https://github.com/lexoyo/gqlfreez/releases/latest/download/gqlfreez-linux-x64 \
  -o /usr/local/bin/gqlfreez && chmod +x /usr/local/bin/gqlfreez

npm install
npm run build     # or: npm start
```

## The whole integration

```
src/_data/api.graphql   ← you write this
src/_data/api.json      ← gqlfreez writes this
```

That is it. **There is no glue code in this project** — no plugin, no `_data/*.js` wrapper,
no async data function, no fetch caching. Eleventy picks up any `.json` in `_data/` as a
[global data file](https://www.11ty.dev/docs/data-global/), so writing the result next to
the query is all it takes for `api.countries` to be available in every template.

`prebuild` runs `gqlfreez` before Eleventy, so `npm run build` does the right thing.

## A page per entry

`src/country.njk` uses the standard Eleventy idiom for turning a data array into pages —
the same one every headless-WordPress tutorial reaches for:

```yaml
pagination:
  data: api.countries
  size: 1
  alias: country
permalink: "/country/{{ country.code | lower }}/"
```

250 countries in, 251 pages out.

## Swapping in your own CMS

Point the endpoint at your own API and change the query. Against WPGraphQL it would read:

```graphql
{
  posts {
    nodes {
      slug
      title
      excerpt
      date
    }
  }
}
```

No `first` argument, so gqlfreez pages through the whole archive rather than stopping at
WPGraphQL's default of 10. Add the token in a `.graphqlrc.yml` and you are done:

```yaml
extensions:
  endpoints:
    default:
      url: https://example.com/graphql
      headers:
        Authorization: "Bearer ${WP_TOKEN}"
```
