# gqlfreez

Freeze GraphQL query results into JSON files, next to the queries.

`gqlfreez` looks through a folder, finds every `.graphql` / `.gql` file, runs the query
against your endpoint, and writes the result to a `.json` file with the same name, right
next to it.

```
src/queries/posts.graphql   →   src/queries/posts.json
```

It runs **before** your build, and it works with any static site generator — Astro,
Eleventy, Hugo, Jekyll, Zola — because it only reads and writes files. There is no plugin
to install and nothing to set up in your generator.

## Install

Download a binary from the [releases page](https://github.com/lexoyo/gqlfreez/releases) —
Linux and macOS on x64 and arm64, Windows on x64. Nothing else to install: no Node, no
Rust, no libraries.

```bash
curl -fsSL https://github.com/lexoyo/gqlfreez/releases/latest/download/gqlfreez-linux-x64 \
  -o /usr/local/bin/gqlfreez && chmod +x /usr/local/bin/gqlfreez
```

On macOS, replace `linux-x64` with `darwin-arm64` (Apple silicon) or `darwin-x64` (Intel).

> `npx gqlfreez` and `cargo install gqlfreez` do **not** work yet — publishing to npm and
> crates.io is still on the roadmap. For now, download the binary.

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

The file holds what was inside `data`, not the whole GraphQL response: it starts with
`{ "posts": … }`, not `{ "data": { "posts": … } }`. Pass `--envelope` to keep the whole
response.

See [`examples/`](./examples) for a working Eleventy site and a working Zola site, both
against a public API you can run right now.

## Pagination

**A field that contains `nodes` or `edges` is a Relay connection.** When you do not ask for
a limit, `gqlfreez` fetches every page and joins the results.

```graphql
{ posts { nodes { title } } }                    # everything
{ posts(first: auto) { nodes { title } } }       # everything, when `first` is required
{ posts(first: 100, after: $cursor) { … } }      # everything, written out in full
{ posts(first: 20) { nodes { title } } }         # twenty. A limit is a limit.
```

Three ways to say "everything", because no single one works on every server:

- **Leave `first` out.** Works on WPGraphQL and most servers.
- **`first: auto`.** GitHub *requires* `first` or `last` on every connection, so leaving it
  out fails there. `auto` never leaves your machine: `gqlfreez` replaces it with
  `--page-size` before sending the query. The downside is that the schema expects an `Int`
  there, so your editor will show an error.
- **Write `after: $cursor` yourself.** Works everywhere and keeps autocompletion, but it is
  longer.

`pageInfo` does not turn pagination on, it is only a field. If you did not ask for it,
`gqlfreez` adds it to paginate and removes it from the output. If you did ask for it, it is
kept and updated to describe the joined result — and `hasNextPage` comes from the last page
actually fetched, never set to `false` to look tidy.

Forward pagination only. Connections next to each other each get their own query. A
connection under a list — `posts { nodes { comments { nodes } } }` — cannot be paginated,
and `gqlfreez` reports an error instead of writing only the first page.

Paging stops at `--max-pages` (20, which is 2000 nodes with the default `--page-size` of
100) and **fails** instead of writing an incomplete file. Raise both for a large archive.

> **Careful with silent limits.** Some servers cut a connection short without saying so:
> WPGraphQL returns 100 nodes for `posts(first: 2000)`, with a normal HTTP 200 and no
> error. `gqlfreez` cannot detect this — `hasNextPage` is the only reliable signal, and it
> is only in the response if the query asked for it. Leave `first` out and you get
> everything.

## Configuration

`gqlfreez` reads your existing [graphql-config](https://the-guild.dev/graphql/config/docs).
Declarative formats only (`.graphqlrc`, `.graphqlrc.{yml,yaml,json,toml}`,
`graphql.config.*`) — a binary cannot run a `.js` / `.ts` config file.

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

Headers under `schema` are what give you autocompletion and schema checking inside
`.graphql` files in VS Code and JetBrains. `${VAR}` and `${VAR:default}` are read from the
environment, and from `.env` / `.env.local` unless you pass `--no-dotenv`.

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

Use a fixed version instead of `latest` if you want repeatable builds: replace
`releases/latest/download` with `releases/download/v0.1.0`.

Useful flags: `--check` fails when a frozen file is out of date, and writes nothing;
`--dry-run` writes nothing at all; `--concurrency` defaults to `1` because shared WordPress
hosting cannot take more; `--delay` waits between two paginated requests.

Exit codes: `0` all good, `1` a query failed, `2` a configuration problem, `3` `--check`
found a file out of date.

## What it does not do yet

**Version 1 only freezes queries that take no parameters.** If you need one query per item
(`post(slug: $slug)`), variables are next on the roadmap.

1. **Publish to npm and crates.io**, so `npx gqlfreez` and `cargo install gqlfreez` work
   (the npm wrapper is written, in `wrappers/node`, but nothing is published yet)
2. Query variables
3. Several named endpoints (`# @endpoint:` per file)
4. Collect every error instead of stopping at the first one
5. Full retry policy (exponential backoff, jitter)
6. Shared fragments with `#import`
7. Backward and nested pagination
8. Merge identical queries across files
9. Service mode (RPC over stdin/stdout) for generator plugins with hot reload

## Prior art

The shape of the tool — a single binary that reads and writes files instead of calling a
generator's API — comes from [Pagebreak](https://github.com/CloudCannon/pagebreak) and
[Pagefind](https://github.com/pagefind/pagefind). That is what lets one binary serve every
ecosystem.

The pagination convention comes from
[`@octokit/plugin-paginate-graphql`](https://github.com/octokit/plugin-paginate-graphql.js)
and from [graphql-fetch-optimizer](https://github.com/internet2000/graphql-fetch-optimizer),
an earlier tool for the same problem.

## License

GPL-3.0
