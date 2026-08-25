# Examples

Two small sites showing the same idea: `gqlfreez` runs **before** the build, the generator
only ever reads plain JSON.

| | |
|---|---|
| [`eleventy/`](./eleventy) | Node-based SSG, `gqlfreez` wired through `prebuild` |
| [`zola/`](./zola) | Rust SSG with no Node at all, `gqlfreez` run from a shell script |

Both point at [countries.trevorblades.com](https://countries.trevorblades.com), a public
GraphQL API that needs no token, so you can run them as they are.
