// Resume window e2e against a running server started with BROWSERSERVE_RESUME_WINDOW_MS.
// Cuts a client abruptly mid-session, resumes with the token, and checks the same page,
// cookie and localStorage are there; resumes with Playwright (strict about stray replies);
// checks expiry and that a used or unknown token is refused.
//   BROWSERSERVE_URL=ws://localhost:9702 WINDOW_MS=15000 npx tsx resume.ts

import { chromium } from "playwright-core";
import puppeteer, { type ConnectionTransport } from "puppeteer-core";
import WebSocket from "ws";

const BASE = process.env.BROWSERSERVE_URL ?? "ws://localhost:9702";
const HTTP = BASE.replace(/^ws/, "http");
const WINDOW_MS = Number(process.env.WINDOW_MS ?? 15000);
const PAGE = "https://en.wikipedia.org/wiki/Web_browser";

let failures = 0;
function check(name: string, ok: boolean, detail = "") {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
  if (!ok) failures += 1;
}
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const pressure = async () => (await (await fetch(`${HTTP}/pressure`)).json()) as { parked: number; running: number };

function openRaw(url: string): Promise<{ ws: WebSocket; token: string | null; status?: number }> {
  return new Promise((resolve) => {
    const ws = new WebSocket(url);
    ws.on("upgrade", (res) => resolve({ ws, token: (res.headers["browserserve-resume-token"] as string) ?? null }));
    ws.on("unexpected-response", (_req, res) => resolve({ ws, token: null, status: res.statusCode }));
    ws.on("error", () => resolve({ ws, token: null, status: 0 }));
  });
}

function transport(ws: WebSocket): ConnectionTransport {
  const t: ConnectionTransport = {
    send: (message) => ws.send(message),
    close: () => ws.close(),
  };
  ws.on("message", (data) => t.onmessage?.(data.toString()));
  ws.on("close", () => t.onclose?.());
  return t;
}

async function waitFor(fn: () => Promise<boolean>, ms: number) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    if (await fn()) return true;
    await sleep(250);
  }
  return false;
}

// 1. Open, do real work, then cut the client with no close frame while a slow command is in flight.
const first = await openRaw(`${BASE}/`);
check("session opens and carries a resume token", Boolean(first.token), first.token ? `${first.token.slice(0, 8)}...` : "none");
const token = first.token ?? "";
const b1 = await puppeteer.connect({ transport: transport(first.ws), protocolTimeout: 30000 });
const p1 = (await b1.pages())[0] ?? (await b1.newPage());
await p1.goto(PAGE, { waitUntil: "domcontentloaded" });
await p1.evaluate(() => {
  document.cookie = "bg_resume=kept; path=/";
  localStorage.setItem("bg_resume", "kept");
});
void p1.evaluate(() => new Promise((r) => setTimeout(r, 3000))).catch(() => undefined);
await sleep(200);
first.ws.terminate();
check("cut session is parked", await waitFor(async () => (await pressure()).parked === 1, 5000));

// 2. Resume with Playwright (asserts on any reply it did not ask for) and find the same page and state.
await sleep(4000);
const pw = await chromium.connectOverCDP(`${BASE}/?resume=${token}`, { timeout: 20000 });
const pages = pw.contexts().flatMap((c) => c.pages());
const same = pages.find((p) => p.url().startsWith(PAGE));
check("resumed browser still has the same page open", Boolean(same), pages.map((p) => p.url()).join(", "));
if (same) {
  const state = await same.evaluate(() => ({ cookie: document.cookie, ls: localStorage.getItem("bg_resume") }));
  check("cookie survived the cut", state.cookie.includes("bg_resume=kept"), state.cookie.slice(0, 80));
  check("localStorage survived the cut", state.ls === "kept", String(state.ls));
  const title = await same.title();
  check("page still works after resume (Playwright, no stray replies)", /Web browser/.test(title), title);
}
check("not parked while a client is attached", (await pressure()).parked === 0);

// 3. A second resume with the same token while attached is refused.
const dup = await openRaw(`${BASE}/?resume=${token}`);
check("token cannot be claimed while a client is attached", dup.status === 404, `status ${dup.status}`);

// 4. Leave cleanly: parks again; after the window it is destroyed and the token is refused.
await pw.close();
check("clean disconnect parks again", await waitFor(async () => (await pressure()).parked === 1, 5000));
await sleep(WINDOW_MS + 2000);
const after = await pressure();
check("browser destroyed after the window", after.parked === 0 && after.running === 0, JSON.stringify(after));
const late = await openRaw(`${BASE}/?resume=${token}`);
check("resume after the window is refused", late.status === 404, `status ${late.status}`);
const unknown = await openRaw(`${BASE}/?resume=${"0".repeat(64)}`);
check("unknown token is refused", unknown.status === 404, `status ${unknown.status}`);

console.log(failures === 0 ? "\nRESUME PASS" : `\nRESUME FAIL (${failures})`);
process.exit(failures === 0 ? 0 : 1);
