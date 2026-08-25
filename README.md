# gqlfreez

Freeze GraphQL query results into JSON files, next to the queries.

`gqlfreez` walks a directory, finds every `.graphql` / `.gql` file, runs the query against
your endpoint, and writes the result to a `.json` file of the same name, right next to it.

```
src/queries/posts.graphql   →   src/queries/posts.json
```

It runs **before** your build, and it works with any static site generator — Astro, Eleventy,
Hugo, Jekyll, Zola — because its contract is the file on disk, not a framework API. There is
no plugin to install and nothing to configure in your generator.

## Install

Download a binary from the [releases page](https://github.com/lexoyo/gqlfreez/releases) —
Linux, macOS and Windows, x64 and arm64. No runtime, no toolchain, no dependencies.

```bash
curl -fsSL https://github.com/lexoyo/gqlfreez/releases/latest/download/gqlfreez-linux-x64 \
  -o /usr/local/bin/gqlfreez && chmod +x /usr/local/bin/gqlfreez
```

macOS: swap `linux-x64` for `darwin-arm64` (Apple silicon) or `darwin-x64` (Intel).

> Publishing to npm and crates.io is on the roadmap, so `npx gqlfreez` and
> `cargo install gqlfreez` do **not** work yet. Until then, the binary is the way in.

## Use

```graphql
# src/queries/posts.graphql
{ posts(first: 10) { nodes { title slug } } }
```

```bash
gqlfreez ./src --endpoint https://example.com/graphql
```

```json
// src/queries/posts.json
{ "posts": { "nodes": [ { "title": "…", "slug": "…" } ] } }
```

The `.json` holds the contents of `data` — you read `data.posts.nodes`, not
`data.data.posts.nodes`. Pass `--envelope` if you want the GraphQL shape back.

See [`examples/`](./examples) for a working Eleventy site and a working Zola site, both
against a public API you can run right now.

## Pagination

**A field whose selection holds `nodes` or `edges` is a Relay connection.** When you do not
express a limit, `gqlfreez` walks every page and merges the results.

```graphql
{ posts { nodes { title } } }                    # everything
{ posts(first: auto) { nodes { title } } }       # everything, where `first` is mandatory
{ posts(first: 100, after: $cursor) { … } }      # everything, the explicit way
{ posts(first: 20) { nodes { title } } }         # twenty. A limit is a limit.
```

Three ways to say "everything", because no single one works everywhere:

- **Leave `first` out.** Works on WPGraphQL and most servers.
- **`first: auto`.** GitHub *requires* `first` or `last` on every connection, so leaving it
  out is an error there. The marker is local — it is replaced by `--page-size` before the
  request, and your server never sees it. The cost: `auto` is an enum where the schema wants
  an `Int`, so your editor will underline it.
- **Write `after: $cursor` yourself.** Valid everywhere, keeps autocompletion, more verbose.

`pageInfo` is not a trigger, just a field. If you did not select it, `gqlfreez` adds it to
paginate and strips it from the output. If you did, it is kept and rebuilt to describe the
merged result — and `hasNextPage` is copied from the last page actually fetched, never
forced to `false`.

Forward pagination only. One connection per query can be nested; several side by side each
get their own derived query.

> **Watch out for silent caps.** Some servers cap connections without telling you: WPGraphQL
> returns 100 nodes for `posts(first: 2000)` with a valid HTTP 200 and no error. `gqlfreez`
> cannot detect that — `hasNextPage` is the only reliable signal, and it is only in the
> response if the query asked for it. Leave `first` out and you get everything.

## Configuration

`gqlfreez` reads your existing [graphql-config](https://the-guild.dev/graphql/config/docs).
Declarative formats only (`.graphqlrc`, `.graphqlrc.{yml,yaml,json,toml}`,
`graphql.config.*`) — a binary cannot evaluate a `.js` / `.ts` config.

```yaml
schema:
  - https://example.com/graphql:
      headers:
        Authorization: "Bearer ${API_TOKEN}"
extensions:
  endpoints:
    default:
      url: https://example.com/graphql
      headers:
        Authorization: "Bearer ${API_TOKEN}"
```

Putting the headers on `schema` is what gives you autocompletion and schema validation
inside `.graphql` files in VS Code and JetBrains. `${VAR}` and `${VAR:default}` are read
from the environment, and from `.env` / `.env.local` unless you pass `--no-dotenv`.

## In CI

```yaml
- name: Install gqlfreez
  run: |
    curl -fsSL https://github.com/lexoyo/gqlfreez/releases/latest/download/gqlfreez-linux-x64 \
      -o /usr/local/bin/gqlfreez
    chmod +x /usr/local/bin/gqlfreez

- name: Freeze the data
  run: gqlfreez ./src
  env:
    API_TOKEN: ${{ secrets.API_TOKEN }}

- run: npm run build     # or hugo, or zola build, or whatever you use
```

Pin a version rather than `latest` if you want reproducible builds — swap
`releases/latest/download` for `releases/download/v0.1.0`.

Useful flags: `--check` fails when a frozen file is out of date (without writing),
`--dry-run` writes nothing at all, `--concurrency` defaults to `1` because shared WordPress
hosting falls over otherwise, `--delay` puts a pause between paginated requests.

Exit codes: `0` fine, `1` a query failed, `2` a configuration problem, `3` `--check` found
something out of date.

## What it does not do yet

**v1 freezes queries that take no parameters.** If you need one query per entity
(`post(slug: $slug)`), variable support is next on the roadmap.

1. **Publish to npm and crates.io**, so `npx gqlfreez` and `cargo install gqlfreez` work
   (the npm wrapper is written, under `wrappers/node`, but nothing is published yet)
2. Query variables
3. Multiple named endpoints (`# @endpoint:` per file)
4. Collecting every failure instead of stopping at the first
5. Full retry policy (exponential backoff, jitter)
6. Shared fragments via `#import`
7. Backward and nested pagination
8. Query merging across files
9. Service mode (stdin/stdout RPC) for SSG plugins with hot reload

## Prior art

The post-processor shape is borrowed from [Pagebreak](https://github.com/CloudCannon/pagebreak)
and [Pagefind](https://github.com/pagefind/pagefind): the contract is the built output, not
a generator's API, so one binary serves every ecosystem.

The pagination convention comes from
[`@octokit/plugin-paginate-graphql`](https://github.com/octokit/plugin-paginate-graphql.js)
and from [graphql-fetch-optimizer](https://github.com/internet2000/graphql-fetch-optimizer),
an earlier take on the same problem.

## License

GPL-3.0
