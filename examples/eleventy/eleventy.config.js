export default function (eleventyConfig) {
  // The query file sits in _data next to its result; it is not a template.
  eleventyConfig.ignores.add("src/_data/*.graphql");

  return {
    dir: { input: "src", output: "_site" },
    markdownTemplateEngine: "njk",
    htmlTemplateEngine: "njk",
  };
}
