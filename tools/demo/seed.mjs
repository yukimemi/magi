// A scratch magi home for the demo take: nothing here touches the operator's
// real home, config, repositories or agents.
//
//   prepare()  makes a temp tree: a git repository with a local bare `origin`
//              (Config::discover reads the base from the remote), a machine
//              config layer whose only agent is the scripted fake one, and an
//              empty MAGI_HOME.
//   seedStage  runs `examples/demo_seed.rs` for one stage of the story. The
//              runs, questions and queue entries are written through magi's own
//              types, so a schema change breaks that example's build rather
//              than recording an empty screen.
import { execFileSync } from "node:child_process";
import { mkdir, mkdtemp, realpath, writeFile } from "node:fs/promises";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
export const ROOT = resolve(HERE, "..", "..");
const EXE = process.platform === "win32" ? ".exe" : "";

/** Where cargo put the release artifacts. */
export function targetDir() {
  return process.env.CARGO_TARGET_DIR
    ? resolve(process.env.CARGO_TARGET_DIR)
    : join(ROOT, "target");
}

export function binaries() {
  const rel = join(targetDir(), "release");
  return {
    magi: join(rel, `magi${EXE}`),
    seeder: join(rel, "examples", `demo_seed${EXE}`),
  };
}

function git(cwd, ...args) {
  execFileSync("git", args, { cwd, stdio: ["ignore", "ignore", "inherit"] });
}

/**
 * The words the recorder types and taps, from the seeder: examples/demo_seed.rs
 * is the one place the demo's content lives, so JS carries no copy of it.
 */
export function strings(lang) {
  return JSON.parse(
    execFileSync(binaries().seeder, ["strings", lang], {
      stdio: ["ignore", "pipe", "inherit"],
    }).toString(),
  );
}

export async function prepare({ lang = "en" } = {}) {
  const words = strings(lang);
  // The request goes into a TOML literal string that teravars renders first.
  if (/['\r\n]|\{\{|\{%/.test(words.request)) {
    throw new Error(`the demo request cannot be written into the config: ${words.request}`);
  }
  // realpath: macOS reports /var as /private/var, and the server compares
  // paths it was handed with the ones it canonicalised.
  const work = await realpath(await mkdtemp(join(tmpdir(), "magi-demo-")));
  const home = join(work, "magi-home");
  const cfgDir = join(work, "config");
  const repo = join(work, "uploader");
  const origin = join(work, "uploader-origin.git");

  // A real home is the one thing this must never write to.
  for (const real of [
    process.env.MAGI_HOME,
    join(homedir(), ".local", "share", "magi"),
    join(homedir(), "Library", "Application Support", "magi"),
  ]) {
    if (real && resolve(real) === resolve(home)) throw new Error("scratch home is a real home");
  }

  await mkdir(repo, { recursive: true });
  await mkdir(join(cfgDir, "magi"), { recursive: true });
  await mkdir(home, { recursive: true });
  git(repo, "init", "-q", "-b", "main");
  git(repo, "config", "user.name", "demo");
  git(repo, "config", "user.email", "demo@example.com");
  await writeFile(join(repo, "README.md"), "# uploader\n");
  git(repo, "add", "-A");
  git(repo, "commit", "-q", "-m", "init");
  git(work, "init", "-q", "--bare", "-b", "main", origin);
  git(repo, "remote", "add", "origin", origin);
  git(repo, "push", "-q", "origin", "main");
  git(repo, "fetch", "-q", "origin");
  // `origin/HEAD` is left unset on purpose, like the test fixture: the
  // resolver recovers it, and that path stays covered.

  const { magi } = binaries();
  // TOML literal strings ('…'): a Windows path keeps its backslashes.
  const q = (s) => `'${s}'`;
  await writeFile(
    join(cfgDir, "magi", "config.toml"),
    [
      "# Demo machine layer: one scripted agent, nothing that reaches out.",
      "[[agents]]",
      'id = "demo"',
      'kind = "command"',
      `command = [${q(process.execPath)}, ${q(join(HERE, "fake-agent.mjs"))}, '{prompt_file}']`,
      `env = { MAGI_BIN = ${q(magi)}, DEMO_REQUEST = ${q(words.request)} }`,
      "",
      "[roles]",
      'chatter = "demo"',
      "",
      "[update]",
      'mode = "off"',
      "",
      "[repos]",
      "fetch_interval = 0",
      "",
      "[daemon]",
      "max_deputies = 0",
      "",
    ].join("\n"),
  );

  // Every child gets these, so no spawned magi can see the operator's setup.
  // Every inherited MAGI_* goes first: recording from inside a magi run
  // (MAGI_RUN, MAGI_NODE, MAGI_WEB_RESUME_LOOP=1 …) would otherwise attribute
  // the task to that run, or start the queue loop in the server.
  const env = Object.fromEntries(
    Object.entries(process.env).filter(([k]) => !k.toUpperCase().startsWith("MAGI_")),
  );
  Object.assign(env, { MAGI_HOME: home, MAGI_CONFIG_DIR: cfgDir, NO_COLOR: "1" });
  return { work, home, cfgDir, repo, env, lang, words };
}

/** inflight | reviewed | question | approval | merged — see examples/demo_seed.rs. */
export function seedStage(scratch, stage) {
  execFileSync(binaries().seeder, [stage, scratch.repo, scratch.lang], {
    env: scratch.env,
    stdio: ["ignore", "ignore", "inherit"],
  });
}
