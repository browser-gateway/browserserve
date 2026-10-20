// Session proxy e2e: real binary, real browser, local proxies with a login.
//   cargo build --release && cd e2e && npm install && npx tsx session-proxy.ts

import { spawn, execFileSync, type ChildProcess } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { createServer as createTlsServer } from "node:tls";
import { createServer, request, type IncomingMessage } from "node:http";
import { connect as tcpConnect, createServer as createTcpServer, type Socket } from "node:net";
import { resolve } from "node:path";
import puppeteer from "puppeteer-core";

const BIN = resolve(import.meta.dirname, "../target/release/browserserve");
const USER = "user";
const PASS = "p@ss:word";

let failures = 0;
function check(name: string, ok: boolean, detail = "") {
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? `  (${detail})` : ""}`);
  if (!ok) failures += 1;
}

const listen = (server: { listen: (port: number, host: string, cb: () => void) => void; address: () => unknown }) =>
  new Promise<number>((done) => server.listen(0, "127.0.0.1", () => done((server.address() as { port: number }).port)));

const target = createServer((_req, res) => res.end("hello-from-target"));
const targetPort = await listen(target);

const seen: string[] = [];
const expectedAuth = `Basic ${Buffer.from(`${USER}:${PASS}`).toString("base64")}`;
const httpProxy = createServer((req: IncomingMessage, res) => {
  seen.push(`${req.method} ${req.url} auth=${req.headers["proxy-authorization"] === expectedAuth}`);
  if (req.headers["proxy-authorization"] !== expectedAuth) {
    res.writeHead(407, { "Proxy-Authenticate": "Basic" }).end();
    return;
  }
  const url = new URL(req.url ?? "");
  const up = request({ host: url.hostname, port: url.port || 80, path: url.pathname + url.search, method: req.method, headers: req.headers }, (r) => {
    res.writeHead(r.statusCode ?? 502, r.headers);
    r.pipe(res);
  });
  up.on("error", () => res.destroy());
  req.pipe(up);
});
httpProxy.on("connect", (req: IncomingMessage, client: Socket, head: Buffer) => {
  seen.push(`CONNECT ${req.url} auth=${req.headers["proxy-authorization"] === expectedAuth}`);
  if (req.headers["proxy-authorization"] !== expectedAuth) {
    client.end("HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic\r\n\r\n");
    return;
  }
  const [host, port] = (req.url ?? "").split(":");
  const up = tcpConnect(Number(port), host, () => {
    client.write("HTTP/1.1 200 Connection Established\r\n\r\n");
    up.write(head);
    up.pipe(client);
    client.pipe(up);
  });
  up.on("error", () => client.destroy());
  client.on("error", () => up.destroy());
});
const httpProxyPort = await listen(httpProxy);

// An https proxy: TLS in front of the http proxy above, certificate signed by a
// throwaway authority that only one browserserve below is told to trust.
const certDir = mkdtempSync(`${tmpdir()}/bs-proxy-tls-`);
const ssl = (args: string[]) => execFileSync("openssl", args, { cwd: certDir, stdio: "ignore" });
ssl(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem", "-days", "1", "-subj", "/CN=test-ca"]);
ssl(["req", "-newkey", "rsa:2048", "-nodes", "-keyout", "proxy.key", "-out", "proxy.csr", "-subj", "/CN=localhost"]);
writeFileSync(`${certDir}/san.ext`, "subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\n");
ssl(["x509", "-req", "-in", "proxy.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-out", "proxy.pem", "-days", "1", "-extfile", "san.ext"]);
const tlsProxy = createTlsServer(
  { key: readFileSync(`${certDir}/proxy.key`), cert: readFileSync(`${certDir}/proxy.pem`) },
  (secure) => {
    const plain = tcpConnect(httpProxyPort, "127.0.0.1");
    secure.pipe(plain);
    plain.pipe(secure);
    plain.on("error", () => secure.destroy());
    secure.on("error", () => plain.destroy());
  },
);
const tlsProxyPort = await listen(tlsProxy);

const socksSeen: string[] = [];
const socksProxy = createTcpServer((s) => {
  let buf = Buffer.alloc(0);
  let stage = 0;
  s.on("data", (d) => {
    buf = Buffer.concat([buf, d]);
    if (stage === 0 && buf.length >= 2 && buf.length >= 2 + buf[1]) {
      buf = buf.subarray(2 + buf[1]);
      s.write(Buffer.from([5, 2]));
      stage = 1;
    }
    if (stage === 1 && buf.length >= 2 && buf.length >= 3 + buf[1] && buf.length >= 3 + buf[1] + buf[2 + buf[1]]) {
      const ulen = buf[1];
      const user = buf.subarray(2, 2 + ulen).toString();
      const plen = buf[2 + ulen];
      const pass = buf.subarray(3 + ulen, 3 + ulen + plen).toString();
      buf = buf.subarray(3 + ulen + plen);
      const ok = user === USER && pass === PASS;
      s.write(Buffer.from([1, ok ? 0 : 1]));
      if (!ok) return void s.end();
      stage = 2;
    }
    const ipv4 = stage === 2 && buf.length >= 10 && buf[3] === 1;
    const named = stage === 2 && buf.length >= 5 && buf[3] === 3 && buf.length >= 7 + buf[4];
    if (ipv4 || named) {
      const host = ipv4 ? [...buf.subarray(4, 8)].join(".") : buf.subarray(5, 5 + buf[4]).toString();
      const at = ipv4 ? 8 : 5 + buf[4];
      const port = buf.readUInt16BE(at);
      const rest = buf.subarray(at + 2);
      socksSeen.push(`${host}:${port}`);
      stage = 3;
      const up = tcpConnect(port, host, () => {
        s.write(Buffer.from([5, 0, 0, 1, 0, 0, 0, 0, 0, 0]));
        if (rest.length) up.write(rest);
        s.removeAllListeners("data");
        up.pipe(s);
        s.pipe(up);
      });
      up.on("error", () => s.destroy());
      s.on("error", () => up.destroy());
    }
  });
});
const socksPort = await listen(socksProxy);

type Server = { proc: ChildProcess; port: number };
async function start(port: number, extra: Record<string, string> = {}): Promise<Server> {
  const proc = spawn(BIN, ["serve"], {
    env: { ...process.env, PORT: String(port), HOST: "127.0.0.1", BROWSERSERVE_CALIBRATE: "0", BROWSERSERVE_PROXY_ALLOW_PRIVATE: "1", ...extra },
    stdio: ["ignore", "ignore", "inherit"],
  });
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    const ready = await fetch(`http://127.0.0.1:${port}/ready`).catch(() => null);
    if (ready?.status === 200) return { proc, port };
    await new Promise((r) => setTimeout(r, 200));
  }
  proc.kill("SIGKILL");
  throw new Error(`server on ${port} never became ready`);
}

function upgrade(port: number, headers: Record<string, string>): Promise<{ status: number; applied: string | undefined; body: string }> {
  return new Promise((done) => {
    const req = request({
      host: "127.0.0.1",
      port,
      path: "/",
      headers: { Connection: "Upgrade", Upgrade: "websocket", "Sec-WebSocket-Version": "13", "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==", ...headers },
    });
    req.on("upgrade", (res, socket) => {
      socket.destroy();
      done({ status: res.statusCode ?? 101, applied: res.headers["browserserve-proxy-applied"] as string | undefined, body: "" });
    });
    req.on("response", (res) => {
      let body = "";
      res.on("data", (c) => (body += c));
      res.on("end", () => done({ status: res.statusCode ?? 0, applied: res.headers["browserserve-proxy-applied"] as string | undefined, body }));
    });
    req.end();
  });
}

const listening = (pid: number) => {
  try {
    return execFileSync("lsof", ["-a", "-p", String(pid), "-iTCP", "-sTCP:LISTEN", "-nP"], { encoding: "utf8" }).trim().split("\n").length - 1;
  } catch {
    return 0;
  }
};

async function visit(port: number, proxy: string | null, url: string): Promise<string> {
  const browser = await puppeteer.connect({
    browserWSEndpoint: `ws://127.0.0.1:${port}/`,
    headers: proxy ? { "Browserserve-Proxy": proxy } : {},
  });
  try {
    const page = await browser.newPage();
    try {
      await page.goto(url, { waitUntil: "domcontentloaded", timeout: 30_000 });
    } catch (e) {
      return `error:${(e as Error).message.split("\n")[0]}`;
    }
    return (await page.evaluate(() => document.body?.innerText ?? "")).trim();
  } finally {
    await browser.disconnect();
  }
}

const enc = encodeURIComponent;
const httpProxyUrl = `http://${enc(USER)}:${enc(PASS)}@127.0.0.1:${httpProxyPort}`;
const server = await start(19331);
try {
  const socksUrl = `socks5://${enc(USER)}:${enc(PASS)}@127.0.0.1:${socksPort}`;

  const version = await (await fetch(`http://127.0.0.1:${server.port}/json/version`)).json();
  check("discovery advertises session proxies", version["Browserserve-Proxy"] === true);

  const up = await upgrade(server.port, { "Browserserve-Proxy": httpProxyUrl });
  check("upgrade with a proxy confirms it", up.status === 101 && up.applied === "1", `${up.status} ${up.applied}`);
  await new Promise((r) => setTimeout(r, 500));
  const plain = await upgrade(server.port, {});
  check("upgrade without a proxy carries no confirmation", plain.status === 101 && plain.applied === undefined);
  await new Promise((r) => setTimeout(r, 500));

  seen.length = 0;
  const text = await visit(server.port, httpProxyUrl, `http://127.0.0.1:${targetPort}/`);
  check("plain http page goes through the proxy with its login", text === "hello-from-target" && seen.some((s) => s.startsWith("GET http://127.0.0.1") && s.endsWith("auth=true")), `${text} | ${seen.join(", ")}`);

  seen.length = 0;
  const wiki = await visit(server.port, httpProxyUrl, "https://en.wikipedia.org/wiki/Proxy_server");
  check("https page tunnels through the proxy with its login", wiki.includes("proxy") && seen.some((s) => s === "CONNECT en.wikipedia.org:443 auth=true"), seen.join(", "));

  const wrong = await visit(server.port, `http://${USER}:wrong@127.0.0.1:${httpProxyPort}`, "https://en.wikipedia.org/wiki/Proxy_server");
  check("a wrong proxy password fails the page, no login prompt", wrong.startsWith("error:net::ERR_TUNNEL_CONNECTION_FAILED"), wrong);

  seen.length = 0;
  const direct = await visit(server.port, null, `http://127.0.0.1:${targetPort}/`);
  check("a session without a proxy goes direct", direct === "hello-from-target" && seen.length === 0, seen.join(", "));

  socksSeen.length = 0;
  const viaSocks = await visit(server.port, socksUrl, `http://127.0.0.1:${targetPort}/`);
  check("socks5 with a login carries the page, host name sent to the proxy", viaSocks === "hello-from-target" && socksSeen.includes(`127.0.0.1:${targetPort}`), `${viaSocks} | ${socksSeen.join(", ")}`);

  const bad = await upgrade(server.port, { "Browserserve-Proxy": "ftp://x:1" });
  check("an unusable proxy address is refused before any browser starts", bad.status === 400 && bad.body.includes("invalid_proxy"), `${bad.status} ${bad.body}`);

} finally {
  server.proc.kill("SIGTERM");
}

const limited = await start(19334, { BROWSERSERVE_MAX_SESSIONS: "1", BROWSERSERVE_MAX_QUEUE: "0" });
try {
  const pid = limited.proc.pid ?? 0;
  const baseline = listening(pid);
  const held = await puppeteer.connect({ browserWSEndpoint: `ws://127.0.0.1:${limited.port}/`, headers: { "Browserserve-Proxy": httpProxyUrl } });
  check("a proxied session holds a listening relay", listening(pid) > baseline, `${listening(pid)} vs ${baseline}`);
  const second = await upgrade(limited.port, { "Browserserve-Proxy": httpProxyUrl });
  check("a proxied session counts against the session limit", second.status === 503, `${second.status} ${second.body}`);
  await held.disconnect();
  await new Promise((r) => setTimeout(r, 1500));
  check("the relay closes when the session ends", listening(pid) === baseline, `${listening(pid)} vs ${baseline}`);
} finally {
  limited.proc.kill("SIGTERM");
}

const off = await start(19332, { BROWSERSERVE_PROXY: "0" });
try {
  const refused = await upgrade(off.port, { "Browserserve-Proxy": `http://127.0.0.1:${httpProxyPort}` });
  check("a server with proxies off refuses instead of running without", refused.status === 400 && refused.body.includes("proxy_unsupported"), `${refused.status} ${refused.body}`);
} finally {
  off.proc.kill("SIGTERM");
}

const trusting = await start(19335, { SSL_CERT_FILE: `${certDir}/ca.pem` });
try {
  const httpsProxyUrl = `https://${enc(USER)}:${enc(PASS)}@localhost:${tlsProxyPort}`;
  seen.length = 0;
  const viaTls = await visit(trusting.port, httpsProxyUrl, "https://en.wikipedia.org/wiki/Proxy_server");
  check("an https proxy with a login carries the session over TLS", viaTls.includes("proxy") && seen.includes("CONNECT en.wikipedia.org:443 auth=true"), seen.join(", "));
} finally {
  trusting.proc.kill("SIGTERM");
}
const untrusting = await start(19336);
try {
  const viaUntrusted = await visit(untrusting.port, `https://${enc(USER)}:${enc(PASS)}@localhost:${tlsProxyPort}`, "https://en.wikipedia.org/wiki/Proxy_server");
  check("an https proxy whose certificate is not trusted is refused", viaUntrusted.startsWith("error:net::ERR_TUNNEL_CONNECTION_FAILED"), viaUntrusted);
} finally {
  untrusting.proc.kill("SIGTERM");
}

const strict = await start(19333, { BROWSERSERVE_PROXY_ALLOW_PRIVATE: "0" });
try {
  const refused = await upgrade(strict.port, { "Browserserve-Proxy": `http://127.0.0.1:${httpProxyPort}` });
  check("a proxy on a private address is refused by default", refused.status === 400 && refused.body.includes("proxy_not_allowed"), `${refused.status} ${refused.body}`);
} finally {
  strict.proc.kill("SIGTERM");
}

target.close();
httpProxy.close();
tlsProxy.close();
socksProxy.close();
console.log(failures === 0 ? "\nALL PASS" : `\n${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
