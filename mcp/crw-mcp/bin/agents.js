// Shared agent registry + installers for `crw-mcp init` (skill only) and
// `crw-mcp install` (skill + MCP server). MCP config shapes are per-agent and
// were verified against each tool's official docs/source — DO NOT "normalize"
// them: Codex is TOML with `mcp_servers`, OpenCode uses `mcp`/`type:local`/
// `command:[]`/`environment`, the rest are JSON `mcpServers`.

const fs = require("fs");
const path = require("path");
const os = require("os");
const { spawnSync } = require("child_process");

const home = os.homedir();

// kind: how the MCP server is registered.
//  - "claude-cli": shell out to `claude mcp add-json` (handles ~/.claude.json)
//  - "json":  merge file[topKey].crw = { [type], command, env }
//  - "toml-codex": append [mcp_servers.crw] to ~/.codex/config.toml
//  - "opencode": merge file.mcp.crw = { type:local, command:[], environment }
const AGENTS = [
  { name: "Claude Code", flag: "--claude-code", configDir: ".claude",
    mcp: { kind: "claude-cli" } },
  { name: "Cursor", flag: "--cursor", configDir: ".cursor",
    mcp: { kind: "json", file: ".cursor/mcp.json", withType: true } },
  { name: "Gemini CLI", flag: "--gemini-cli", configDir: ".gemini",
    mcp: { kind: "json", file: ".gemini/settings.json", withType: false } },
  { name: "Codex", flag: "--codex", configDir: ".codex",
    mcp: { kind: "toml-codex", file: ".codex/config.toml" } },
  { name: "OpenCode", flag: "--opencode", configDir: ".opencode",
    mcp: { kind: "opencode", file: path.join(".config", "opencode", "opencode.json") } },
  { name: "Windsurf", flag: "--windsurf", configDir: ".codeium",
    mcp: { kind: "json", file: path.join(".codeium", "windsurf", "mcp_config.json"), withType: false } },
];

function detectAgents(args) {
  const has = (f) => args.includes(f);
  const explicit = AGENTS.filter((a) => has(a.flag));
  if (explicit.length) return explicit;
  // --all or no flag → every agent whose config dir exists.
  return AGENTS.filter((a) => fs.existsSync(path.join(home, a.configDir)));
}

function getApiKey(args) {
  const i = args.indexOf("--api-key");
  return i !== -1 && args[i + 1] ? args[i + 1] : process.env.CRW_API_KEY || null;
}

function readSkill() {
  return fs.readFileSync(path.join(__dirname, "..", "skills", "SKILL.md"), "utf-8");
}

/** Write the SKILL.md into the agent's skills/crw/ dir. Returns the path. */
function installSkill(agent, skillContent) {
  const dir = path.join(home, agent.configDir, "skills", "crw");
  fs.mkdirSync(dir, { recursive: true });
  const file = path.join(dir, "SKILL.md");
  atomicWriteText(file, skillContent);
  return file;
}

function readJson(file) {
  if (!fs.existsSync(file)) return {};
  try {
    const value = JSON.parse(fs.readFileSync(file, "utf-8"));
    if (!value || typeof value !== "object" || Array.isArray(value)) {
      throw new Error("top-level value must be an object");
    }
    return value;
  } catch (error) {
    throw new Error(`refusing to overwrite malformed JSON in ${file}: ${error.message}`);
  }
}

function ensureObject(value, label, file) {
  if (value === undefined) return {};
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error(`refusing to replace non-object ${label} in ${file}`);
  }
  return value;
}

function backupOnce(file) {
  if (!fs.existsSync(file)) return;
  const backup = `${file}.crw-backup`;
  if (!fs.existsSync(backup)) fs.copyFileSync(file, backup);
}

function atomicWriteText(file, content) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  backupOnce(file);
  const tmp = `${file}.crw-tmp-${process.pid}-${Date.now()}`;
  try {
    fs.writeFileSync(tmp, content, { encoding: "utf-8", mode: 0o600 });
    fs.renameSync(tmp, file);
  } finally {
    try { fs.unlinkSync(tmp); } catch { /* already renamed or never created */ }
  }
}

function writeJson(file, obj) {
  atomicWriteText(file, JSON.stringify(obj, null, 2) + "\n");
}

/**
 * Register the crw MCP server for one agent. `env` is {} for local mode (the
 * embedded binary needs no config) or {CRW_API_KEY, CRW_API_URL} for cloud.
 * Returns a short human label of what was done.
 */
// Spawn the MCP server via `npx -y crw-mcp` rather than a bare `crw-mcp`. A
// fresh `npx crw-mcp install` leaves NO `crw-mcp` on PATH (npx is ephemeral),
// so a bare command would give the agent a server it can't launch. npx resolves
// the launcher each spawn (first run downloads the native binary), so the
// config works with zero extra install steps. Matches the SaaS "MCP config"
// docs. Users who prefer the fast native binary can still swap in `crw-mcp`.
const MCP_CMD = "npx";
const MCP_ARGS = ["-y", "crw-mcp"];

function installMcp(agent, env) {
  const m = agent.mcp;
  const hasEnv = Object.keys(env).length > 0;

  if (m.kind === "claude-cli") {
    // add-json avoids the `-e` variadic-name pitfall and merges ~/.claude.json
    // (user scope) correctly. Probe before touching state, then restore the
    // exact prior user config if replacement fails.
    const server = { command: MCP_CMD, args: MCP_ARGS, ...(hasEnv ? { env } : {}) };
    const probe = spawnSync("claude", ["--version"], { stdio: "ignore" });
    if (probe.error || probe.status !== 0) {
      throw new Error("Claude Code CLI was detected but is not launchable");
    }
    const configFile = path.join(home, ".claude.json");
    const original = fs.existsSync(configFile) ? fs.readFileSync(configFile) : null;
    spawnSync("claude", ["mcp", "remove", "--scope", "user", "crw"], { stdio: "ignore" });
    const r = spawnSync(
      "claude",
      ["mcp", "add-json", "--scope", "user", "crw", JSON.stringify(server)],
      { stdio: "ignore" },
    );
    if (r.error || r.status !== 0) {
      if (original) {
        fs.writeFileSync(configFile, original);
      } else {
        try { fs.unlinkSync(configFile); } catch { /* no file was created */ }
      }
      throw new Error("Claude Code rejected the MCP registration; previous config restored");
    }
    return "MCP via claude CLI (user scope)";
  }

  if (m.kind === "json") {
    const file = path.join(home, m.file);
    const cfg = readJson(file);
    cfg.mcpServers = ensureObject(cfg.mcpServers, "mcpServers", file);
    cfg.mcpServers.crw = {
      ...(m.withType ? { type: "stdio" } : {}),
      command: MCP_CMD,
      args: MCP_ARGS,
      ...(hasEnv ? { env } : {}),
    };
    writeJson(file, cfg);
    return `MCP → ${m.file}`;
  }

  if (m.kind === "opencode") {
    const file = path.join(home, m.file);
    const cfg = readJson(file);
    if (!cfg.$schema) cfg.$schema = "https://opencode.ai/config.json";
    cfg.mcp = ensureObject(cfg.mcp, "mcp", file);
    cfg.mcp.crw = {
      type: "local",
      command: [MCP_CMD, ...MCP_ARGS],
      ...(hasEnv ? { environment: env } : {}),
    };
    writeJson(file, cfg);
    return `MCP → ${m.file}`;
  }

  if (m.kind === "toml-codex") {
    const file = path.join(home, m.file);
    let toml = "";
    try {
      toml = fs.readFileSync(file, "utf-8");
    } catch {
      /* new file */
    }
    // The installer owns the crw section. Replace it so a previous install
    // with embedded env credentials cannot override a later `--from-config`
    // setup (and so re-runs remain idempotent).
    toml = removeOwnedCodexSections(toml);
    const envLines = hasEnv
      ? "\n[mcp_servers.crw.env]\n" +
        Object.entries(env).map(([k, v]) => `${k} = "${v}"`).join("\n") + "\n"
      : "";
    const block = `\n[mcp_servers.crw]\ncommand = "${MCP_CMD}"\nargs = [${MCP_ARGS.map((a) => `"${a}"`).join(", ")}]\n${envLines}`;
    const next = toml + (toml && !toml.endsWith("\n") ? "\n" : "") + block;
    atomicWriteText(file, next);
    return `MCP → ${m.file}`;
  }

  return "MCP: unsupported";
}

function removeOwnedCodexSections(toml) {
  const lines = toml.split("\n");
  const kept = [];
  let skipping = false;
  for (const line of lines) {
    const isSection = /^\s*\[[^\]]+\]/.test(line);
    const isOwned = /^\s*\[mcp_servers\.crw(?:\.[^\]]+)?\]\s*(?:#.*)?$/.test(line);
    if (isOwned) {
      skipping = true;
      continue;
    }
    if (isSection) skipping = false;
    if (!skipping) kept.push(line);
  }
  return kept.join("\n").replace(/\n{3,}/g, "\n\n").trimEnd();
}

module.exports = { AGENTS, detectAgents, getApiKey, readSkill, installSkill, installMcp, home };
