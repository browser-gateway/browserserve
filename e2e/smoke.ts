// browserserve e2e smoke: puppeteer-core + playwright-core against a running instance.
//   cd e2e && npm install && BROWSERSERVE_URL=ws://localhost:9333 npm run smoke
// With auth: additionally set BROWSERSERVE_TOKEN to the server's token.

import { chromium } from "playwright-core";
import puppeteer from "puppeteer-core";

const BASE_WS = process.env.BROWSERSERVE_URL ?? "ws://localhost:9333";
const TOKEN = process.env.BROWSERSERVE_TOKEN;
const HTTP_URL = BASE_WS.replace(/^ws/, "http");
const WS_URL = TOKEN ? `${BASE_WS}/?token=${TOKEN}` : BASE_WS;

let failures = 0;
function check(name: string, ok: boolean, detail = "") {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
  if (!ok) failures += 1;
}

async function pressure(): Promise<{ running: number; warm: number }> {
  const res = await fetch(`${HTTP_URL}/pressure`);
  return res.json() as Promise<{ running: number; warm: number }>;
}

async function waitForRunning(target: number, budgetMs = 15000): Promise<number> {
  const deadline = Date.now() + budgetMs;
  let running = (await pressure()).running;
  while (running !== target && Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 200));
    running = (await pressure()).running;
  }
  return running;
}

async function authChecks() {
  if (!TOKEN) return;
  const bare = await fetch(`${HTTP_URL}/json/version`);
  check("auth: discovery without token is rejected", bare.status === 401);

  const authed = await fetch(`${HTTP_URL}/json/version?token=${TOKEN}`);
  check("auth: discovery with token succeeds", authed.status === 200);
  const body = (await authed.json()) as { webSocketDebuggerUrl: string };
  check(
    "auth: advertised ws url carries the token",
    body.webSocketDebuggerUrl.includes("token="),
  );

  const rejected = await puppeteer
    .connect({ browserWSEndpoint: BASE_WS })
    .then(() => false)
    .catch(() => true);
  check("auth: ws connect without token is rejected", rejected);
}

async function puppeteerChecks() {
  const before = await pressure();
  check("baseline reachable", typeof before.running === "number");

  const a = await puppeteer.connect({ browserWSEndpoint: WS_URL });
  const b = await puppeteer.connect({ browserWSEndpoint: WS_URL });
  check("two concurrent sessions connect", true);

  const during = await pressure();
  check("pressure counts 2 running", during.running === 2, `running=${during.running}`);

  const pageA = await a.newPage();
  await pageA.goto("data:text/html,<title>alpha</title><h1>session A</h1>");
  const pageB = await b.newPage();
  await pageB.goto("data:text/html,<title>beta</title><h1>session B</h1>");

  check("session A sees its own page", (await pageA.title()) === "alpha");
  check("session B sees its own page", (await pageB.title()) === "beta");

  const shot = await pageA.screenshot({ type: "png" });
  check("screenshot works over the bridge", shot.length > 1000, `${shot.length} bytes`);

  const versionA = await a.version();
  check("CDP version call works", versionA.includes("Chrome"), versionA);

  a.disconnect();
  b.disconnect();
  const running = await waitForRunning(0);
  check("sessions destroyed after disconnect", running === 0, `running=${running}`);
  check("warm stock replenished", (await pressure()).warm >= 1);
}

async function playwrightChecks() {
  const url = TOKEN ? `${HTTP_URL}/?token=${TOKEN}` : HTTP_URL;
  const browser = await chromium.connectOverCDP(url);
  const context = browser.contexts()[0] ?? (await browser.newContext());
  const page = await context.newPage();
  await page.goto("data:text/html,<title>pw</title>ok");
  check("playwright connectOverCDP + navigate", (await page.title()) === "pw");
  await browser.close();
  const running = await waitForRunning(0);
  check("playwright session cleaned after close", running === 0, `running=${running}`);
}

async function main() {
  await authChecks();
  await puppeteerChecks();
  await playwrightChecks();
  console.log(failures === 0 ? "\nSMOKE PASS" : `\nSMOKE FAIL (${failures})`);
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error("SMOKE ERROR", e);
  process.exit(1);
});
