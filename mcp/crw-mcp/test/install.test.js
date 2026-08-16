const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const install = path.join(__dirname, "..", "bin", "install.js");

function runInstall(home) {
  return spawnSync(process.execPath, [install, "--codex"], {
    encoding: "utf8",
    env: {
      ...process.env,
      HOME: home,
      USERPROFILE: home,
      XDG_CONFIG_HOME: path.join(home, ".config"),
      CRW_API_KEY: "",
    },
  });
}

test("install registers a launchable local MCP server in a clean Codex home", (t) => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "crw-mcp-install-"));
  t.after(() => fs.rmSync(home, { recursive: true, force: true }));

  const first = runInstall(home);
  assert.equal(first.status, 0, first.stderr || first.stdout);

  const skill = fs.readFileSync(
    path.join(home, ".codex", "skills", "crw", "SKILL.md"),
    "utf8",
  );
  assert.match(skill, /^---\nname: crw\n/m);

  const configPath = path.join(home, ".codex", "config.toml");
  const config = fs.readFileSync(configPath, "utf8");
  assert.match(config, /\[mcp_servers\.crw\]/);
  assert.match(config, /command = "npx"/);
  assert.match(config, /args = \["-y", "crw-mcp"\]/);
  assert.doesNotMatch(config, /CRW_API_KEY/);

  const second = runInstall(home);
  assert.equal(second.status, 0, second.stderr || second.stdout);
  const configAfterSecondInstall = fs.readFileSync(configPath, "utf8");
  assert.equal(
    (configAfterSecondInstall.match(/\[mcp_servers\.crw\]/g) || []).length,
    1,
    "re-running install must not duplicate the MCP registration",
  );
});
