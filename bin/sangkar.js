#!/usr/bin/env node
"use strict";

// The launcher the npm package ships: resolve the one platform package, hand it every argument byte
// for byte, and exit with its code. There is nothing else to it — the server, the indexer and the
// seven tools are the native binary, so this file is the only JavaScript the package carries.
//
// `require` comes from `createRequire` rather than a static import or the global one: a `.js` file is
// a module while `package.json` says `"type": "module"` and CommonJS once that field goes, and the
// launcher is the one runtime that must not care which of the two is true. Its base is
// `process.argv[1]` realpath'd, so the symlink npm puts in the global bin directory resolves the
// module tree the package was installed into instead of the one beside the symlink.

// One entry per published platform, keyed by `<platform>:<arch>`. Adding macOS or Windows later is
// one line here, one `npm/<key>/` directory and one row in the release matrix — never a rewrite.
const PLATFORMS = { "linux:x64": "sangkar-mcp-linux-x64-gnu" };

// Where the refusal sends a caller whose platform or install has no binary: the last TypeScript
// build on the registry, which still answers everywhere. It moves when a newer one is published.
const TYPESCRIPT_PIN = "0.6.0-beta.1";

// What is published, spelled from the table so the line and the install cannot drift apart.
const PUBLISHED = Object.entries(PLATFORMS)
  .map(([key, name]) => `${key.replace(":", " ")} (${name})`)
  .join(", ");

// One line and exit 70 (`EX_SOFTWARE`) naming the wanted key, the published platform and the pin.
// Both causes land here — a platform with no package, and an install that skipped the optional one.
const refuse = (key) => {
  process.stderr.write(
    `sangkar: no native binary for "${key}"; the only published platform is ${PUBLISHED}; ` +
      `pin sangkar-mcp@${TYPESCRIPT_PIN} to stay on the TypeScript build\n`,
  );
  process.exit(70);
};

const main = async () => {
  const key = `${process.platform}:${process.arch}`;
  const name = PLATFORMS[key];
  if (!name) return refuse(key);

  const [fs, { createRequire }] = await Promise.all([import("node:fs"), import("node:module")]);
  const require = createRequire(fs.realpathSync(process.argv[1]));

  let manifest;
  try {
    manifest = require.resolve(`${name}/package.json`);
  } catch {
    return refuse(key);
  }

  const path = require("node:path");
  const binary = path.join(path.dirname(manifest), "bin", "sangkar");
  const { spawn } = require("node:child_process");

  // `spawn`, not `spawnSync`: forwarding a signal needs the child handle before it exits, and
  // `spawnSync` hands back the pid only once the child is gone. `inherit` keeps every descriptor the
  // caller's own, so the stdio protocol is never interleaved by a pipe of ours.
  const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });

  for (const signal of ["SIGINT", "SIGTERM"]) process.on(signal, () => child.kill(signal));

  child.on("error", (error) => {
    process.stderr.write(`sangkar: cannot run ${binary}: ${error.message}\n`);
    process.exit(70);
  });

  // `exit`, not `close`: stdio is inherited, so the child leaves nothing of its own to drain.
  child.on("exit", (code, signal) => {
    // A child killed by a signal has no code to forward, so report it the way a shell would.
    if (code !== null) return process.exit(code);
    process.exit(128 + (require("node:os").constants.signals[signal] ?? 0));
  });
};

main().catch((error) => {
  process.stderr.write(`sangkar: launcher failed: ${error?.message ?? error}\n`);
  process.exit(70);
});
