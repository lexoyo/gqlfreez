# gqlfreez + Zola

```bash
./build.sh
```

Zola is a single Rust binary with no Node runtime, which is exactly the case a
generator-specific plugin cannot serve. `gqlfreez` writes `data/countries.json` and Zola
reads it with its built-in `load_data`.

That is the whole point of a post-processor: the contract is the file on disk, so the
same query file works here and in the Eleventy example next door.
