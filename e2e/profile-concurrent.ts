// Several profile sessions starting at once must each start with their saved cookie.
// Fresh browsers launched together may not have opened their first page when seeding starts.
//   BROWSERSERVE_URL=ws://localhost:9702 ROUNDS=4 N=5 npx tsx profile-concurrent.ts

import puppeteer from "puppeteer-core";

const WS = process.env.BROWSERSERVE_URL ?? "ws://localhost:9702";
const HTTP = WS.replace(/^ws/, "http");
const ROUNDS = Number(process.env.ROUNDS ?? 4);
const N = Number(process.env.N ?? 5);

let failures = 0;
function check(name: string, ok: boolean, detail = "") {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
  if (!ok) failures += 1;
}

async function dropOff(value: string): Promise<string> {
  const cookie = {
    name: "bg_seed",
    value,
    domain: ".wikipedia.org",
    path: "/",
    expires: Math.floor(Date.now() / 1000) + 3600,
    size: 0,
    httpOnly: false,
    secure: true,
    session: false,
    sameSite: "Lax",
  };
  const res = await fetch(`${HTTP}/v1/profile`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ cookies: [cookie], localStorage: [], indexeddb: [] }),
  });
  return ((await res.json()) as { profileToken: string }).profileToken;
}

async function seededValue(value: string): Promise<string | null> {
  const token = await dropOff(value);
  const browser = await puppeteer.connect({ browserWSEndpoint: `${WS}/?profileToken=${token}`, protocolTimeout: 30000 });
  try {
    const cdp = await browser.target().createCDPSession();
    const { cookies } = (await cdp.send("Storage.getCookies")) as { cookies: Array<{ name: string; value: string }> };
    return cookies.find((c) => c.name === "bg_seed")?.value ?? null;
  } finally {
    await browser.disconnect();
  }
}

for (let round = 1; round <= ROUNDS; round += 1) {
  const values = Array.from({ length: N }, (_, i) => `r${round}-s${i}`);
  const results = await Promise.allSettled(values.map((v) => seededValue(v)));
  const seeded = results.filter((r, i) => r.status === "fulfilled" && r.value === values[i]).length;
  const detail = results
    .map((r) => (r.status === "fulfilled" ? String(r.value) : `error: ${String(r.reason).slice(0, 60)}`))
    .join(", ");
  check(`round ${round}: ${N} concurrent profile sessions each start with their own cookie`, seeded === N, detail);
  await new Promise((r) => setTimeout(r, 3000));
}

console.log(failures === 0 ? "\nPROFILE-CONCURRENT PASS" : `\nPROFILE-CONCURRENT FAIL (${failures})`);
process.exit(failures === 0 ? 0 : 1);
