#!/usr/bin/env node
const os = require("os");
const { spawnSync } = require("child_process");

const execname = "gqlfreez";

function resolveBinaryPath() {
  const override = process.env[`${execname.toUpperCase()}_BINARY_PATH`];
  if (override) return override;

  const cpu = process.env.npm_config_arch || os.arch();
  const platform = process.platform === "win32" ? "windows" : process.platform;
  const executable = platform === "windows" ? `${execname}.exe` : execname;

  try {
    return require.resolve(`@${execname}/${platform}-${cpu}/bin/${executable}`);
  } catch (e) {
    console.error(
      [
        `Failed to find the ${execname} binary for ${platform}-${cpu}.`,
        ``,
        `If npm skipped the optional dependency (a known npm issue with lockfiles`,
        `generated on another platform), try: rm -rf node_modules package-lock.json && npm i`,
        ``,
        `Otherwise the platform may not be supported yet. Please open an issue at`,
        `https://github.com/lexoyo/${execname}/issues and paste this message in full,`,
        `or download a release binary from`,
        `https://github.com/lexoyo/${execname}/releases and point ${execname.toUpperCase()}_BINARY_PATH at it.`,
      ].join("\n"),
    );
    process.exit(1);
  }
}

try {
  const result = spawnSync(resolveBinaryPath(), process.argv.slice(2), {
    windowsHide: true,
    stdio: [process.stdin, process.stdout, process.stderr],
  });
  process.exit(result.status ?? 1);
} catch (err) {
  console.error(`Failed to run ${execname} via the npm wrapper: ${err}`);
  process.exit(1);
}
