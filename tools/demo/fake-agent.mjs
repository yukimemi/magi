// The demo's only "agent": a scripted stand-in for the chat's CLI.
//
// magi drives it as a `kind = "command"` agent (see seed.mjs), so the chat turn
// goes through the real path: briefing with the selected persona, the agent
// runs, its stdout is the reply. The one thing it does besides talking is what
// a real chat agent would do — file the task with the real `magi task add`
// (MAGI_RUN / MAGI_NODE are inherited from the turn, so the queue records it
// as filed by the chat).
//
// No network, no CLI of a vendor. Argument: the prompt file.
import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";

// The operator's request, from the same seeder strings the recorder types
// (seed.mjs puts it in this agent's env), so the filed task always matches.
const INSTRUCTION = process.env.DEMO_REQUEST;
const prompt = readFileSync(process.argv[2], "utf8");

const fail = (why) => {
  console.error(`demo fake agent: ${why}`);
  process.exit(1);
};

// The take is only worth recording if the persona really reached the prompt.
if (!/Persona \(tone only\)/.test(prompt) || !/asuka/i.test(prompt)) {
  fail("the Asuka persona is not in the prompt");
}
if (!INSTRUCTION) fail("DEMO_REQUEST is not set");
if (!prompt.includes(INSTRUCTION)) fail("the operator's request is not in the prompt");

const magi = process.env.MAGI_BIN;
if (!magi) fail("MAGI_BIN is not set");
execFileSync(magi, ["task", "add", INSTRUCTION], { stdio: ["ignore", "ignore", "inherit"] });

// Asuka's voice. The first line is the one the operator asked for by name; the
// rest stays short so the whole reply fits a phone frame without scrolling.
console.log(
  [
    "あんたバカァ！",
    "こんなリトライ、あたしに任せなさい。",
    "タスクは登録したわ。感謝しなさいよね！",
  ].join("\n\n"),
);
