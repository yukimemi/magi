// Records assets/demo.gif: a scripted, re-recordable take of the phone UI.
//
//   node record.mjs            record and encode assets/demo.gif
//   node record.mjs --shots    one PNG per beat in target/demo-shots, no encode
//   node record.mjs --headed   watch it happen; --keep keeps the scratch tree
//
// Run through `cargo make demo-gif` / `demo-shots`, which build the release
// binary first. Node, not bun: playwright talks CDP over extra stdio pipes (fd
// 3 and 4) that bun's child_process does not carry, so under bun the browser
// launches, nothing connects, and it fails minutes later with a launch timeout.
//
// The story: hand a task over from the phone, watch the blind competition,
// answer the agent's question, approve the merge. Everything on screen comes
// from real state and real requests:
//   - the chat turn runs a scripted `kind = "command"` agent (fake-agent.mjs),
//     which files the task with the real `magi task add`;
//   - runs / questions are written by examples/demo_seed.rs through magi's types;
//   - every tap goes through the real UI to the real server.
// The one seeded transition is the end: no forge and no land loop run offline,
// so after the approval tap is saved, `merged` is written by the seeder.
// No DOM is ever injected.
import { spawn, execFileSync } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdir, rm, stat, writeFile } from "node:fs/promises";
import { createConnection, createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { setTimeout as wait } from "node:timers/promises";
import { chromium } from "playwright-core";
import { ROOT, binaries, prepare, seedStage } from "./seed.mjs";

const argv = new Set(process.argv.slice(2));
const SHOTS = argv.has("--shots");
const HEADED = argv.has("--headed");
const KEEP = argv.has("--keep");

// A phone-shaped frame. The capture is 2x so the text stays crisp; the encode
// scales it to OUT_WIDTH.
const WIDTH = 390;
const HEIGHT = 700;
const SCALE = 2;
const OUT_WIDTH = 360;
const COLORS = 128;
const MAX_BYTES = 3 * 1024 * 1024;

// Ports other services on the operator's machine own (AGENTS.md, "Never take a
// port you did not check"). A floor, not a list: the port is also probed.
const OCCUPIED = new Set([8080, 8188, 8788, 8789, 8791, 4222, 8222, 6123, 6124, 7878]);

const log = (msg) => console.error(`demo: ${msg}`);

// ---------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------

function connects(port) {
  return new Promise((done) => {
    const s = createConnection({ port, host: "127.0.0.1" });
    s.once("connect", () => (s.destroy(), done(true)));
    s.once("error", () => done(false));
  });
}

/** An OS-chosen port, then proven free and off the occupied list. */
async function freePort() {
  for (let i = 0; i < 20; i++) {
    const port = await new Promise((done, fail) => {
      const s = createServer();
      s.once("error", fail);
      s.listen(0, "127.0.0.1", () => {
        const { port } = s.address();
        s.close(() => done(port));
      });
    });
    if (!OCCUPIED.has(port) && !(await connects(port))) return port;
  }
  throw new Error("no free port found");
}

async function startServer(scratch) {
  const { magi } = binaries();
  if (!existsSync(magi)) {
    throw new Error(`${magi} is missing — run \`cargo make demo-gif\` (it builds it)`);
  }
  const port = await freePort();
  const child = spawn(
    magi,
    ["web", "--bind", "127.0.0.1", "--port", String(port), "--repo", scratch.repo],
    { env: scratch.env, stdio: ["ignore", "ignore", "pipe"], cwd: scratch.repo },
  );
  const err = [];
  child.stderr.on("data", (d) => err.push(d.toString()));
  let exited = false;
  child.on("exit", () => (exited = true));

  const base = `http://127.0.0.1:${port}`;
  // The port was free a moment ago; whatever answers must be OUR server, on
  // OUR scratch home, before anything is driven.
  for (let i = 0; i < 150; i++) {
    if (exited) throw new Error(`magi web exited early:\n${err.join("")}`);
    try {
      const health = await (await fetch(`${base}/api/health`)).json();
      if (health.home !== scratch.home) {
        throw new Error(`port ${port} answers for another home: ${health.home}`);
      }
      // Nothing may run the queue: a loop would start real attempts on the
      // task the chat files and fight the seeded run.
      const loop = await (await fetch(`${base}/api/loop`)).json();
      if (loop.running) throw new Error("the server is running the queue loop");
      return { child, base };
    } catch (e) {
      if (/another home|queue loop/.test(String(e.message))) throw e;
    }
    await wait(200);
  }
  throw new Error(`magi web did not come up:\n${err.join("")}`);
}

function stopServer(child) {
  if (child.exitCode !== null) return;
  if (process.platform === "win32") {
    try {
      execFileSync("taskkill", ["/pid", String(child.pid), "/T", "/F"], { stdio: "ignore" });
    } catch {}
  } else {
    child.kill("SIGTERM");
  }
}

// ---------------------------------------------------------------------
// The browser
// ---------------------------------------------------------------------

const CHROME_CANDIDATES = [
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
  "/usr/bin/google-chrome",
  "/usr/bin/chromium",
  "/usr/bin/chromium-browser",
  "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
];

async function launchBrowser() {
  // MAGI_CHROME first, like tests/web_render.rs; otherwise playwright's own.
  const exe =
    process.env.MAGI_CHROME ||
    (process.env.DEMO_SYSTEM_CHROME ? CHROME_CANDIDATES.find(existsSync) : undefined);
  try {
    return await chromium.launch({ headless: !HEADED, executablePath: exe });
  } catch (err) {
    if (!/Executable doesn't exist|playwright install/i.test(String(err?.message))) throw err;
    throw new Error(
      "chromium is not installed for playwright. Run:\n" +
        "  node node_modules/playwright-core/cli.js install chromium\n" +
        "from tools/demo, set MAGI_CHROME to a browser binary, or just use\n" +
        "`cargo make demo-gif`, which installs it.\n\n" +
        `Original error: ${err.message}`,
    );
  }
}

// ---------------------------------------------------------------------
// The capture
// ---------------------------------------------------------------------

/**
 * Lossless PNG frames off the compositor, with their own timestamps.
 * `recordVideo` writes VP8, which changes a pixel or two everywhere on a
 * motionless screen — noise a gif cannot collapse (the same trap as yaiba's).
 */
async function startScreencast(page) {
  const cdp = await page.context().newCDPSession(page);
  const frames = [];
  cdp.on("Page.screencastFrame", (event) => {
    frames.push({
      at: event.metadata?.timestamp ?? Date.now() / 1000,
      png: Buffer.from(event.data, "base64"),
    });
    cdp.send("Page.screencastFrameAck", { sessionId: event.sessionId }).catch(() => {});
  });
  await cdp.send("Page.startScreencast", {
    format: "png",
    everyNthFrame: 1,
    maxWidth: WIDTH * SCALE,
    maxHeight: HEIGHT * SCALE,
  });
  return async () => {
    await cdp.send("Page.stopScreencast").catch(() => {});
    await cdp.detach().catch(() => {});
    return frames;
  };
}

/** Frames closer than this (s) are dropped: animations paint at the display rate. */
const MIN_GAP = 0.04;

function thin(frames) {
  const kept = [frames[0]];
  for (const frame of frames.slice(1)) {
    if (frame.at - kept.at(-1).at >= MIN_GAP) kept.push(frame);
  }
  if (kept.at(-1) !== frames.at(-1)) kept.push(frames.at(-1));
  return kept;
}

/** Drop the frames inside each cut window and close the gap in the timeline. */
function applyCuts(frames, cuts) {
  const out = [];
  for (const f of frames) {
    if (cuts.some((c) => f.at >= c.from && f.at < c.to)) continue;
    const shift = cuts.filter((c) => c.to <= f.at).reduce((n, c) => n + (c.to - c.from), 0);
    out.push({ ...f, at: f.at - shift });
  }
  return out;
}

/** The concat list: each frame holds for as long as it was on screen. */
async function writeFrames(frames, dir, tailSeconds) {
  if (frames.length < 2) throw new Error(`screencast produced ${frames.length} frames`);
  await mkdir(dir, { recursive: true });
  const lines = [];
  for (let i = 0; i < frames.length; i++) {
    const name = `f${String(i).padStart(5, "0")}.png`;
    await writeFile(join(dir, name), frames[i].png);
    const next = frames[i + 1];
    const seconds = next ? next.at - frames[i].at : tailSeconds;
    lines.push(`file '${name}'`, `duration ${Math.max(seconds, 1 / 60).toFixed(4)}`);
  }
  lines.push(`file 'f${String(frames.length - 1).padStart(5, "0")}.png'`);
  const list = join(dir, "frames.txt");
  await writeFile(list, lines.join("\n"));
  return list;
}

function ffmpeg(args) {
  return new Promise((done, fail) => {
    const p = spawn("ffmpeg", args, { stdio: ["ignore", "ignore", "pipe"] });
    const err = [];
    p.stderr.on("data", (d) => err.push(d.toString()));
    p.on("error", () => fail(new Error("ffmpeg not found on PATH — install it and re-run")));
    p.on("exit", (code) =>
      code === 0 ? done() : fail(new Error(`ffmpeg failed:\n${err.join("")}`)),
    );
  });
}

/** Two passes: a palette from the whole take, then the gif that uses it. */
async function encode(list, gif) {
  const palette = join(dirname(list), "palette.png");
  const chain = `scale=${OUT_WIDTH}:-1:flags=lanczos`;
  const input = ["-f", "concat", "-safe", "0", "-i", list];
  await ffmpeg([
    "-y", ...input,
    "-vf", `${chain},palettegen=max_colors=${COLORS}:stats_mode=diff`,
    palette,
  ]);
  await ffmpeg([
    "-y", ...input, "-i", palette,
    "-lavfi", `${chain}[x];[x][1:v]paletteuse=dither=none:diff_mode=rectangle`,
    "-fps_mode", "passthrough",
    "-loop", "0",
    gif,
  ]);
}

// ---------------------------------------------------------------------
// The storyboard
// ---------------------------------------------------------------------

const REQUEST = "add retry with backoff to the uploader";
const QUOTE = "あんたバカァ！";

async function storyboard(page, scratch, base, beat, quiet) {
  // Scroll the target to mid-screen first: at the edge the dock sits on top
  // of it and the tap lands on the dock instead.
  // The UI re-renders a view when its data lands, which can replace the
  // element between the look and the tap: look again rather than fail.
  const tap = async (locator) => {
    for (let tries = 0; ; tries++) {
      try {
        await locator.evaluate((el) => el.scrollIntoView({ block: "center", behavior: "instant" }), null, { timeout: 5000 });
        await wait(150);
        await locator.tap({ force: true, timeout: 5000 });
        return;
      } catch (e) {
        if (tries >= 4) throw e;
        await wait(300);
      }
    }
  };
  const dock = (name) => page.locator(`.dock-item[data-nav="${name}"]`);
  const scrollTo = (y) =>
    page.evaluate((top) => window.scrollTo({ top, behavior: "instant" }), y);
  const questions = async () => (await fetch(`${base}/api/questions`)).json();

  // 0. Chat, in Asuka's voice. Everything is typed and tapped for real; the
  //    reply is the scripted agent's, and it files the task itself.
  // (The page was opened on #/chat before the capture began.)
  await page.waitForSelector("#talk-start-go");
  await beat("chat-empty", 500);
  // The list settles (and shifts the button) after the first paint; tap
  // again if the conversation did not open.
  await page.waitForFunction(() => !/Loading/.test(document.querySelector("#talks-count").textContent));
  await quiet(async () => {
    for (let tries = 0; ; tries++) {
      await tap(page.locator("#talk-start-go"));
      const opened = await page
        .waitForSelector("#view-talk:not([hidden])", { timeout: 4000 })
        .then(() => true, () => false);
      if (opened) break;
      if (tries >= 3) throw new Error("the conversation never opened");
    }
  }, 300);
  // The persona list comes with the conversation's detail. If that fetch lost
  // a race with the conversation being created, load the page again.
  for (let tries = 0; ; tries++) {
    const listed = await page
      .waitForFunction(
        () => !!document.querySelector('#talk-persona option[value="asuka"]'),
        null,
        { timeout: 5000 },
      )
      .then(() => true, () => false);
    if (listed) break;
    if (tries >= 2) throw new Error("the persona list never loaded");
    await page.reload();
  }
  await page.selectOption("#talk-persona", "asuka");
  // The switch is saved by the server and the page re-renders when it lands;
  // typing before that would lose the text.
  await page.locator("#talk-turns").getByText("persona changed").first().waitFor();
  await beat("persona", 500);
  await tap(page.locator("#f-talk-say"));
  for (let tries = 0; ; tries++) {
    await page.keyboard.type(REQUEST, { delay: 14 });
    if ((await page.locator("#f-talk-say").inputValue()) === REQUEST) break;
    if (tries >= 2) throw new Error("the message was not typed");
    await page.locator("#f-talk-say").fill("");
  }
  await beat("typed", 200);
  // The send only counts once the request shows up as a turn.
  for (let tries = 0; ; tries++) {
    await tap(page.locator("#talk-send"));
    const sent = await page
      .locator("#talk-turns")
      .getByText(REQUEST)
      .first()
      .waitFor({ timeout: 5000 })
      .then(() => true, () => false);
    if (sent) break;
    if (tries >= 3) throw new Error("the message was never sent");
  }
  await quiet(async () => {
    await page.locator("#talk-turns").getByText(QUOTE).waitFor({ timeout: 30_000 });
    await page.locator("#talk-wait").waitFor({ state: "hidden", timeout: 10_000 });
  }, 1000);
  // Long enough to read the line the operator asked for.
  await beat("reply", 2600);

  // 1. The task the chat filed, in the real queue.
  await quiet(async () => {
    await tap(dock("queue"));
    await page.locator("#queue-sections").getByText(REQUEST).first().waitFor();
    await scrollTo(0);
  }, 200);
  await beat("queue", 1200);

  // 2. The run in flight: three candidates, no agent names.
  const card = page.locator("#view-runs a.card.run-card").first();
  await quiet(async () => {
    seedStage(scratch, "inflight");
    await tap(dock("runs"));
    await card.waitFor();
    await scrollTo(0);
  }, 200);
  await beat("runs", 1000);
  await quiet(async () => {
    await tap(card);
    await page.waitForSelector("#view-run:not([hidden])");
    await scrollTo(0);
  }, 200);
  await beat("run-inflight", 1000);

  // 3. Ranking, tally, winner and the review rounds closing.
  await quiet(async () => {
    seedStage(scratch, "reviewed");
    await wait(600);
  }, 150);
  await beat("run-reviewed", 900);
  await scrollTo(760);
  await beat("run-ranking", 1000);
  await scrollTo(1500);
  // Open the review rounds for real: round 1's finding, fixed in round 2.
  await tap(page.locator("summary", { hasText: "Reviews" }).first());
  await wait(200);
  await beat("run-reviews", 1500);

  // 4. A question from an agent, answered with a tap.
  const choice = page.getByRole("button", { name: "SQLite" }).first();
  await quiet(async () => {
    seedStage(scratch, "question");
    await tap(dock("questions"));
    await choice.waitFor();
    await choice.evaluate((el) => el.scrollIntoView({ block: "center", behavior: "instant" }));
  }, 200);
  await beat("question", 1200);
  await tap(choice);
  await beat("answered", 800);

  // 5. The merge approval: the real two-step tap.
  const arm = page.getByRole("button", { name: /Merge this pull request/ }).first();
  await quiet(async () => {
    seedStage(scratch, "approval");
    await arm.waitFor({ timeout: 20_000 });
    await scrollTo(0);
  }, 200);
  await beat("approval", 1000);
  await page
    .locator("iframe")
    .first()
    .evaluate((el) => el.scrollIntoView({ block: "center", behavior: "instant" }));
  await beat("panel", 1100);
  await tap(arm);
  const yes = page.getByRole("button", { name: "Yes, merge now" });
  await yes.waitFor();
  await beat("confirm", 700);
  await tap(yes);
  // The answer is saved by the real API; only then does the (seeded) merge
  // land, because no forge or land loop exists here.
  await quiet(async () => {
    for (let i = 0; i < 50; i++) {
      const done = (await questions()).some(
        (q) => q.node === "land-approval" && q.status === "answered",
      );
      if (done) return;
      await wait(200);
    }
    throw new Error("the merge approval was never saved");
  }, 200);
  await quiet(async () => {
    seedStage(scratch, "merged");
    await tap(dock("runs"));
    await tap(page.locator("#view-runs").getByText(/^Done\s*\d*$/).first());
    const done = page.locator("#view-runs a.card.run-card").first();
    await done.waitFor();
    await tap(done);
    await page.waitForSelector("#view-run:not([hidden])");
    await scrollTo(0);
  }, 250);
  await beat("merged", 1500);
}

async function main() {
  const scratch = await prepare();
  let server;
  let browser;
  try {
    server = await startServer(scratch);
    log(`serving ${server.base} on ${scratch.home}`);
    browser = await launchBrowser();
    const shotDir = join(ROOT, "target", "demo-shots");
    if (SHOTS) {
      await rm(shotDir, { recursive: true, force: true });
      await mkdir(shotDir, { recursive: true });
    }

    const context = await browser.newContext({
      viewport: { width: WIDTH, height: HEIGHT },
      deviceScaleFactor: SCALE,
      isMobile: true,
      hasTouch: true,
      colorScheme: "dark",
      locale: "en-US",
    });
    const page = await context.newPage();
    await page.goto(`${server.base}/#/chat`);
    await page.waitForLoadState("networkidle");
    await page.waitForFunction(
      () => !/Loading/.test(document.querySelector("#talks-count").textContent),
    );
    page.on("pageerror", (e) => log(`page error: ${e.message}`));
    const stop = SHOTS ? null : await startScreencast(page);

    const cuts = [];
    let n = 0;
    const t0 = Date.now();
    const beat = async (name, hold = 0) => {
      if (hold) await wait(hold);
      if (SHOTS) {
        await page.screenshot({
          path: join(shotDir, `${String(n++).padStart(2, "0")}-${name}.png`),
        });
      }
      const cut = cuts.reduce((n, c) => n + (c.to - c.from), 0);
      log(`beat ${name} @${((Date.now() - t0) / 1000 - cut).toFixed(1)}s`);
    };
    // Dead time the viewer should not sit through (a real agent turn, a page
    // load): the window after `keep` ms is cut from the take and the frames
    // after it move up. What was on screen is untouched; only waiting is cut.
    const quiet = async (fn, keep = 700) => {
      const from = Date.now() / 1000 + keep / 1000;
      const out = await fn();
      const to = Date.now() / 1000;
      if (to > from) cuts.push({ from, to });
      return out;
    };
    try {
      await storyboard(page, scratch, server.base, beat, quiet);
    } catch (e) {
      // What the page showed is the first thing a failed take needs.
      const text = await page.locator("main").innerText().catch(() => "?");
      log(`failed on: ${text.slice(0, 500).replace(/\n+/g, " | ")}`);
      throw e;
    }

    if (SHOTS) {
      await context.close();
      log(`shots in ${shotDir}`);
      return;
    }
    const captured = await stop();
    const frames = applyCuts(thin(captured), cuts);
    await context.close();
    const list = await writeFrames(frames, join(scratch.work, "frames"), 1.2);
    log(`captured ${captured.length} frames, encoding ${frames.length}`);
    const gif = join(ROOT, "assets", "demo.gif");
    await encode(list, gif);
    const { size } = await stat(gif);
    log(`wrote ${gif} (${(size / 1024 / 1024).toFixed(2)} MB)`);
    if (size > MAX_BYTES) log("WARNING: over 3 MB — shorten a beat or lower COLORS / OUT_WIDTH");
  } finally {
    if (browser) await browser.close().catch(() => {});
    if (server) stopServer(server.child);
    if (!KEEP) await rm(scratch.work, { recursive: true, force: true }).catch(() => {});
  }
}

await main();
