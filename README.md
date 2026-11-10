# gqlfreez

Freeze GraphQL query results into JSON files, next to the queries.

`gqlfreez` looks through a folder, finds every `.graphql` / `.gql` file, runs the query against your endpoint, and writes the result to a `.json` file with the same name, right next to it.

```
src/queries/posts.graphql   →   src/queries/posts.json
```

It runs **before** your build, and it works with any static site generator — Astro, Eleventy, Hugo, Jekyll, Zola — because it only reads and writes files. There is no plugin to install and nothing to set up in your generator.

## Install

Download a binary from the [releases page](https://github.com/lexoyo/gqlfreez/releases) — Linux and macOS on x64 and arm64, Windows on x64. Nothing else to install: no Node, no Rust, no libraries.

```bash
curl -fsSL https://github.com/lexoyo/gqlfreez/releases/latest/download/gqlfreez-linux-x64 \
  -o /usr/local/bin/gqlfreez && chmod +x /usr/local/bin/gqlfreez
```

On macOS, replace `linux-x64` with `darwin-arm64` (Apple silicon) or `darwin-x64` (Intel).

> `npx gqlfreez` and `cargo install gqlfreez` do **not** work yet — publishing to npm and crates.io is still on the roadmap. For now, download the binary.

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

The file holds what was inside `data`, not the whole GraphQL response: it starts with `{ "posts": … }`, not `{ "data": { "posts": … } }`. Pass `--envelope` to keep the whole response.

See [`examples/`](./examples) for a working Eleventy site and a working Zola site, both against a public API you can run right now.

## Pagination

**A field that contains `nodes` or `edges` is a Relay connection.** When you do not ask for a limit, `gqlfreez` fetches every page and joins the results.

```graphql
{ posts { nodes { title } } }                    # everything
{ posts(first: auto) { nodes { title } } }       # everything, when `first` is required
{ posts(first: 100, after: $cursor) { … } }      # everything, written out in full
{ posts(first: 20) { nodes { title } } }         # twenty. A limit is a limit.
```

Three ways to say "everything", because no single one works on every server:

- **Leave `first` out.** Works on WPGraphQL and most servers.
- **`first: auto`.** GitHub *requires* `first` or `last` on every connection, so leaving it out fails there. `auto` never leaves your machine: `gqlfreez` replaces it with `--page-size` before sending the query. The downside is that the schema expects an `Int` there, so your editor will show an error.
- **Write `after: $cursor` yourself.** Works everywhere and keeps autocompletion, but it is longer.

`pageInfo` does not turn pagination on, it is only a field. If you did not ask for it, `gqlfreez` adds it to paginate and removes it from the output. If you did ask for it, it is kept and updated to describe the joined result — and `hasNextPage` comes from the last page actually fetched, never set to `false` to look tidy.

Forward pagination only. Connections next to each other each get their own query. A connection under a list — `posts { nodes { comments { nodes } } }` — cannot be paginated, and `gqlfreez` reports an error instead of writing only the first page.

Paging stops at `--max-pages` (20, which is 2000 nodes with the default `--page-size` of 100) and **fails** instead of writing an incomplete file. Raise both for a large archive.

> **Careful with silent limits.** Some servers cut a connection short without saying so: WPGraphQL returns 100 nodes for `posts(first: 2000)`, with a normal HTTP 200 and no error. `gqlfreez` cannot detect this — `hasNextPage` is the only reliable signal, and it is only in the response if the query asked for it. Leave `first` out and you get everything.

## Collections: one file per item

Some generators, Zola and Jekyll for example, only create pages from files. For these generators, `gqlfreez` can also write one file per item of a list, with the item in a YAML front matter. One query produces all the pages. Your generator then builds from local files only, with no network access.

To enable this, add an entry under `extensions.collections` in your config file (`.graphqlrc.yml`, see [Configuration](#configuration)). If you have several config files, use the one nearest to the query file. The key is the path of the query file, relative to the folder of the config file, with forward slashes. No command-line flag is needed.

```yaml
# .graphqlrc.yml, at the site root
schema: https://blog.example.com/graphql
extensions:
  collections:
    data/posts.graphql:
      list: posts.nodes            # where the list is, in the result
      dir: content/blog            # where to write, relative to this file's folder
      file: "{{item.slug}}.md"     # the name of each file
      frontMatter:
        title: "{{item.title}}"
        path: "/blog/{{item.slug}}/"
        weight: "{{index}}"
        extra:
          post: "{{item}}"
```

```yaml
# content/blog/hello-world.md
---
# generated by gqlfreez from ../../data/posts.graphql, do not edit
title: 'Hello world'
path: '/blog/hello-world/'
weight: 0
extra:
  post:
    slug: 'hello-world'
    title: 'Hello world'
---
```

- **One entry per query.** The key is the path of the query file, from the folder of the config file. `gqlfreez` stops with a configuration error if the key matches no query file that it finds, or if the entry is in a config that is not the nearest one to the query. An unknown setting is an error too. JSON and TOML configs use the same keys.
- **Values are inserted with [Mustache](https://mustache.github.io/) tags**, written `{{ }}` and rendered by [Handlebars](https://handlebarsjs.com/). `{{item.slug}}` inserts a field of the item, `{{item}}` the whole item, and `{{index}}` its position in the list, starting at 0. A number selects a list element: `{{item.nodes.0.title}}`. For literal braces, write `\{{`. In YAML and TOML, put that string between single quotes, so that the backslash is kept.
- **A string made of a single tag keeps the type of the value**, an object or a number for example. In any other string, the value is converted to text.
- **The run fails** if two items produce the same file name, ignoring case, or if the list itself is `null`: nothing is written or deleted then. A `null` list can happen with `--allow-partial`, which keeps the data when the server also returns errors.
- **Null values are removed everywhere in the front matter**, also inside lists (the list gets shorter). Zola converts a YAML front matter to TOML, which has no null. All strings are quoted, so Jekyll reads `NO`, `on` or `1.0` as text.
- **Only generated files are deleted.** Each generated file contains the line `# generated by gqlfreez from <query>, do not edit`. On the next run, a file is deleted if it was generated from this query and the query no longer produces it. `_index.md` (Zola/Hugo section file) and your own files are never touched. If a page would overwrite a file that was not generated from this query, the run fails.
- **Nothing is written unless everything succeeded.** If any query or collection fails, no JSON file and no page is written. Only a disk error during the final write can leave some files updated. Unchanged pages are not rewritten. The summary printed by `--check` and `--dry-run` counts pages too.
- **Never outside the scanned directory.** `dir` must stay inside the directory passed to `gqlfreez` (`Options.root` when used as a library). If pages go to `content/`, pass the site root to `gqlfreez`, not `data/`.
- **Use a stable field for `file`.** With `{{index}}.md`, reordering the list rewrites every page.

The [Zola example](./examples/zola) uses this to write one page per country.

## Configuration

`gqlfreez` reads your existing [graphql-config](https://the-guild.dev/graphql/config/docs). Declarative formats only (`.graphqlrc`, `.graphqlrc.{yml,yaml,json,toml}`, `graphql.config.*`) — a binary cannot run a `.js` / `.ts` config file.

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

Headers under `schema` are what give you autocompletion and schema checking inside `.graphql` files in VS Code and JetBrains. `${VAR}` and `${VAR:default}` are read from the environment, and from `.env` / `.env.local` unless you pass `--no-dotenv`.

### Several data sources

Each `.graphql` file uses the closest config. This lets one run freeze several APIs, each with its own URL and headers:

```
data/
  wp/
    .graphqlrc.yml      # url: https://blog.example.com/graphql
    posts.graphql
  shop/
    .graphqlrc.yml      # url: https://shop.example.com/graphql
    products.graphql
```

```bash
gqlfreez .
```

- The config is searched in the file's folder, then in each parent folder up to the directory you passed, then above it.
- With `--endpoint`, only that URL and the `--header` values are used: no URL or header comes from a config, so no token from a config is ever sent to that endpoint.
- The configs are always read for their collections, so a config that cannot be read, for example because a `${VAR}` in it is not set, is an error (exit code 2), even with `--endpoint`.
- Without `--endpoint`, `--header` is added to every source.

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

Use a fixed version instead of `latest` if you want repeatable builds: replace `releases/latest/download` with `releases/download/v0.1.0`.

Useful flags: `--check` fails when a frozen file is out of date, and writes nothing; `--dry-run` writes nothing at all; `--concurrency` defaults to `1` because shared WordPress hosting cannot take more; `--delay` waits between two paginated requests.

Exit codes: `0` all good, `1` a query failed, `2` a configuration problem (including an error in a collection, such as an unknown setting, a key that names no query, or a `dir` outside the scanned directory), `3` `--check` found a file out of date.

If no query file is found, the CLI reports a configuration problem. The library function `run` returns `Error::NoQueries`, so you can tell an empty site from a broken setup.

## What it does not do yet

**Queries cannot take parameters yet**, apart from the `$cursor` of [pagination](#pagination), which `gqlfreez` fills in. To get one page per item, use a [collection](#collections-one-file-per-item): a single query fetches the whole list. Query variables (`post(slug: $slug)`) are on the roadmap, for cases where a list query cannot return everything you need.

1. **Publish to npm and crates.io**, so `npx gqlfreez` and `cargo install gqlfreez` work (the npm wrapper is written, in `wrappers/node`, but nothing is published yet)
2. Query variables
3. Collect every error instead of stopping at the first one
4. Full retry policy (exponential backoff, jitter)
5. Shared fragments with `#import`
6. Backward and nested pagination
7. Merge identical queries across files
8. Service mode (RPC over stdin/stdout) for generator plugins with hot reload

## Prior art

The shape of the tool — a single binary that reads and writes files instead of calling a generator's API — comes from [Pagebreak](https://github.com/CloudCannon/pagebreak) and [Pagefind](https://github.com/pagefind/pagefind). That is what lets one binary serve every ecosystem.

The pagination convention comes from [`@octokit/plugin-paginate-graphql`](https://github.com/octokit/plugin-paginate-graphql.js) and from [graphql-fetch-optimizer](https://github.com/internet2000/graphql-fetch-optimizer), an earlier tool for the same problem.

## License

EUPL-1.2
