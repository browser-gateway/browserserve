// Browser half of metadata-block.sh: a real page loads, every metadata address fails.
import puppeteer from "puppeteer-core";

const WS = `${process.env.BROWSERSERVE_URL}/?token=${process.env.BROWSERSERVE_TOKEN}`;
let failures = 0;
function check(name: string, ok: boolean, detail = "") {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
  if (!ok) failures += 1;
}

const browser = await puppeteer.connect({ browserWSEndpoint: WS });
const page = await browser.newPage();
await page.goto("https://en.wikipedia.org/wiki/Web_browser", { waitUntil: "domcontentloaded", timeout: 30_000 });
check("chrome: public page loads", (await page.title()).includes("Wikipedia"), await page.title());

for (const url of ["http://169.254.169.254/computeMetadata/v1/", "http://metadata.google.internal/", "http://2852039166/", "http://[::ffff:169.254.169.254]/"]) {
  const err = await page.goto(url, { timeout: 10_000 }).then(() => "loaded", (e: Error) => e.message);
  const refused = err.includes("ERR_CONNECTION_REFUSED") || (url.includes("metadata.google.internal") && err.includes("ERR_NAME_NOT_RESOLVED"));
  check(`chrome: navigation to ${url} is refused by the firewall`, refused, err.slice(0, 80));
}
const blank = await browser.newPage();
const probe = (url: string) => fetch(url, { mode: "no-cors" }).then(() => "connected", (e: Error) => e.message);
const control = await blank.evaluate(probe, "https://en.wikipedia.org/");
check("chrome: no-cors fetch control reaches a public site", control === "connected", control);
const fetched = await blank.evaluate(probe, "http://169.254.169.254/computeMetadata/v1/");
check("chrome: fetch() to metadata fails", fetched !== "connected", fetched);
await browser.close();
process.exit(failures ? 1 : 0);
