// Profile sessions with the resume window on: a cut holds the browser, a resume finds the
// same page, and the profile is captured once, when the session finally ends (a deliberate
// close, or the window running out with no one returning). Read-only sessions resume and
// never capture. A long session keeps its capture while other profile sessions come and go.
//   BROWSERSERVE_URL=ws://localhost:9702 WINDOW_MS=15000 npx tsx profile-resume.ts

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

interface Captured {
  cookies: Array<{ name: string; value: string }>;
  localStorage: Array<{ origin: string; items: Array<[string, string]> | Array<{ key: string; value: string }> }>;
}

async function dropOff(): Promise<string> {
  const res = await fetch(`${HTTP}/v1/profile`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ cookies: [], localStorage: [], indexeddb: [] }),
  });
  return ((await res.json()) as { profileToken: string }).profileToken;
}

async function pickUp(profileToken: string): Promise<Captured | null> {
  const res = await fetch(`${HTTP}/v1/profile/${profileToken}`);
  return res.ok ? ((await res.json()) as Captured) : null;
}

async function pickUpWithin(profileToken: string, ms: number): Promise<Captured | null> {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    const got = await pickUp(profileToken);
    if (got) return got;
    await sleep(500);
  }
  return null;
}

function openRaw(url: string): Promise<{ ws: WebSocket; token: string | null; status?: number }> {
  return new Promise((resolve) => {
    const ws = new WebSocket(url);
    ws.on("upgrade", (res) => resolve({ ws, token: (res.headers["browserserve-resume-token"] as string) ?? null }));
    ws.on("unexpected-response", (_req, res) => resolve({ ws, token: null, status: res.statusCode }));
    ws.on("error", () => resolve({ ws, token: null, status: 0 }));
  });
}

function transport(ws: WebSocket): ConnectionTransport {
  const t: ConnectionTransport = { send: (m) => ws.send(m), close: () => ws.close() };
  ws.on("message", (data) => t.onmessage?.(data.toString()));
  ws.on("close", () => t.onclose?.());
  return t;
}

async function setCookie(ws: WebSocket, value: string): Promise<void> {
  const browser = await puppeteer.connect({ transport: transport(ws), protocolTimeout: 30000 });
  const page = (await browser.pages()).find((p) => p.url().startsWith(PAGE)) ?? (await browser.newPage());
  if (!page.url().startsWith(PAGE)) await page.goto(PAGE, { waitUntil: "domcontentloaded" });
  await page.evaluate((v) => {
    document.cookie = `bg_${v}=1; path=/; max-age=3600`;
    localStorage.setItem(`bg_${v}`, "1");
  }, value);
}

const cookieNames = (c: Captured | null) => (c?.cookies ?? []).map((x) => x.name).filter((n) => n.startsWith("bg_"));

// 1. Cut, resume, write more, close on purpose: one capture with both writes.
if (process.env.ONLY_LONG !== "1") {
  const profileToken = await dropOff();
  const first = await openRaw(`${BASE}/?profileToken=${profileToken}`);
  check("profile session carries a resume token", Boolean(first.token));
  await setCookie(first.ws, "before");
  first.ws.terminate();
  await sleep(1500);
  check("nothing captured at the cut", (await pickUp(profileToken)) === null);
  const second = await openRaw(`${BASE}/?resume=${first.token}`);
  check("profile session resumes", second.status === undefined, `status ${second.status}`);
  const browser = await puppeteer.connect({ transport: transport(second.ws), protocolTimeout: 30000 });
  const page = (await browser.pages()).find((p) => p.url().startsWith(PAGE));
  check("same page after resume", Boolean(page));
  await setCookie(second.ws, "after");
  second.ws.close(1000);
  const captured = await pickUpWithin(profileToken, 15000);
  const names = cookieNames(captured);
  check("captured once at the end, with writes from before and after the cut", names.includes("bg_before") && names.includes("bg_after"), names.join(","));
  check("captured only once", (await pickUp(profileToken)) === null);
}

// 2. Cut, never return: captured when the window runs out.
if (process.env.ONLY_LONG !== "1") {
  const profileToken = await dropOff();
  const first = await openRaw(`${BASE}/?profileToken=${profileToken}`);
  await setCookie(first.ws, "abandoned");
  first.ws.terminate();
  await sleep(WINDOW_MS / 2);
  check("held, not captured, inside the window", (await pickUp(profileToken)) === null);
  const captured = await pickUpWithin(profileToken, WINDOW_MS + 15000);
  check("captured after the window with no reconnect", cookieNames(captured).includes("bg_abandoned"), cookieNames(captured).join(","));
}

// 3. Read-only: resumable, never captured.
if (process.env.ONLY_LONG !== "1") {
  const profileToken = await dropOff();
  const first = await openRaw(`${BASE}/?profileToken=${profileToken}&readOnly=1`);
  check("read-only session carries a resume token", Boolean(first.token));
  await setCookie(first.ws, "readonly");
  first.ws.terminate();
  await sleep(1500);
  const second = await openRaw(`${BASE}/?resume=${first.token}`);
  check("read-only session resumes", second.status === undefined, `status ${second.status}`);
  second.ws.close(1000);
  await sleep(4000);
  check("read-only session captures nothing", (await pickUp(profileToken)) === null);
}

// 4. A session longer than the hand-off store's 2-minute window keeps its capture while other profile sessions come and go.
if (process.env.LONG === "1") {
  const profileToken = await dropOff();
  const first = await openRaw(`${BASE}/?profileToken=${profileToken}`);
  await setCookie(first.ws, "long");
  const until = Date.now() + 130_000;
  while (Date.now() < until) {
    const other = await dropOff();
    const o = await openRaw(`${BASE}/?profileToken=${other}&readOnly=1`);
    o.ws.close(1000);
    await sleep(20_000);
  }
  first.ws.close(1000);
  const captured = await pickUpWithin(profileToken, 15000);
  check("a session past two minutes still hands back its profile", cookieNames(captured).includes("bg_long"), cookieNames(captured).join(","));
}

console.log(failures === 0 ? "\nPROFILE-RESUME PASS" : `\nPROFILE-RESUME FAIL (${failures})`);
process.exit(failures === 0 ? 0 : 1);
