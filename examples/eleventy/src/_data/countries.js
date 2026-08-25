// Eleventy global data: just read the JSON gqlfreez wrote. No API call, no async,
// no network at build time.
import data from "../queries/countries.json" with { type: "json" };

export default () => data.countries;
