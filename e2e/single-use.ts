// Single-use mode e2e: spawns the real binary with a real browser and checks
// one session is served, every later connect is refused, and the process exits 0.
//   cargo build --release && cd e2e && npm install && npx tsx single-use.ts

import { spawn, type ChildProcess } from "node:child_process";
import { request } from "node:http";
import { resolve } from "node:path";
import puppeteer from "puppeteer-core";

const BIN = resolve(import.meta.dirname, "../target/release/browserserve");
const TOKEN = "single-use-e2e";

let failures = 0;
function check(name: string, ok: boolean, detail = "") {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
  if (!ok) failures += 1;
}

type Server = { proc: ChildProcess; port: number; exited: Promise<number | null> };

async function start(port: number): Promise<Server> {
  const proc = spawn(BIN, ["serve"], {
    env: {
      ...process.env,
      PORT: String(port),
      HOST: "127.0.0.1",
      BROWSERSERVE_TOKEN: TOKEN,
      BROWSERSERVE_SINGLE_USE: "1",
      BROWSERSERVE_MIN_READY: "1",
    },
    stdio: ["ignore", "ignore", "inherit"],
  });
  const exited = new Promise<number | null>((done) => proc.on("exit", (code) => done(code)));
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    const ready = await fetch(`http://127.0.0.1:${port}/ready`).catch(() => null);
    if (ready?.status === 200) return { proc, port, exited };
    await new Promise((r) => setTimeout(r, 200));
  }
  proc.kill("SIGKILL");
  throw new Error(`server on ${port} never became ready`);
}

function upgradeStatus(port: number, query: string): Promise<{ status: number; body: string }> {
  return new Promise((done) => {
    const req = request({
      host: "127.0.0.1",
      port,
      path: `/?${query}`,
      headers: {
        Connection: "Upgrade",
        Upgrade: "websocket",
        "Sec-WebSocket-Version": "13",
        "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
      },
    });
    req.on("upgrade", (res, socket) => {
      socket.destroy();
      done({ status: res.statusCode ?? 101, body: "" });
    });
    req.on("response", (res) => {
      let body = "";
      res.on("data", (chunk) => (body += chunk));
      res.on("end", () => done({ status: res.statusCode ?? 0, body }));
    });
    req.on("error", (e) => done({ status: 0, body: String(e) }));
    req.end();
  });
}

async function exitCodeWithin(server: Server, ms: number): Promise<number | null | "timeout"> {
  return Promise.race([
    server.exited,
    new Promise<"timeout">((r) => setTimeout(() => r("timeout"), ms)),
  ]);
}

async function servedSessionThenExit() {
  console.log("== one session served, the next refused, then exit 0 ==");
  const server = await start(9471);
  const bad = await upgradeStatus(server.port, "token=wrong");
  check("a bad token is refused and does not spend the instance", bad.status === 401);
  const readyAfterBad = await fetch(`http://127.0.0.1:${server.port}/ready`);
  check("still ready after the refused bad token", readyAfterBad.status === 200);

  const browser = await puppeteer.connect({
    browserWSEndpoint: `ws://127.0.0.1:${server.port}/?token=${TOKEN}`,
  });
  const page = await browser.newPage();
  await page.goto("data:text/html,<title>single-use</title>");
  check("first session drives a page", (await page.title()) === "single-use");

  const second = await upgradeStatus(server.port, `token=${TOKEN}`);
  check(
    "second connect is refused while the first is live",
    second.status === 503 && second.body.includes("single_use_spent"),
    `${second.status} ${second.body}`,
  );
  const ready = await fetch(`http://127.0.0.1:${server.port}/ready`);
  const readyBody = (await ready.json()) as { spent?: boolean };
  check("ready reports not-ready and spent", ready.status === 503 && readyBody.spent === true);

  await browser.disconnect();
  const code = await exitCodeWithin(server, 20_000);
  check("process exits 0 after the session ends", code === 0, `exit=${code}`);
  if (code === "timeout") server.proc.kill("SIGKILL");
}

async function failedClaimStillExits() {
  console.log("== a spent instance whose claim failed still exits ==");
  const server = await start(9472);
  const res = await upgradeStatus(server.port, `token=${TOKEN}&profileToken=does-not-exist`);
  check("unknown profile token is refused", res.status === 404, String(res.status));
  const code = await exitCodeWithin(server, 20_000);
  check("process exits 0 even though no session ran", code === 0, `exit=${code}`);
  if (code === "timeout") server.proc.kill("SIGKILL");
}

async function sigtermStillWorks() {
  console.log("== SIGTERM before any session still shuts down cleanly ==");
  const server = await start(9473);
  server.proc.kill("SIGTERM");
  const code = await exitCodeWithin(server, 20_000);
  check("process exits 0 on SIGTERM while unspent", code === 0, `exit=${code}`);
  if (code === "timeout") server.proc.kill("SIGKILL");
}

await servedSessionThenExit();
await failedClaimStillExits();
await sigtermStillWorks();
console.log(failures === 0 ? "ALL PASS" : `${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
