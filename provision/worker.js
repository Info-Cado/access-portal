// Access Portal sign-up service (a Cloudflare Worker).
//
// Each Access Portal install calls this once (and again on every start) with
// its install id. The first call creates a Cloudflare Tunnel for the install, a
// public name under ZONE_NAME (e.g. khush-laptop.infocado.in) pointing at it,
// and routes that name to the portal on the install's machine. Later calls
// return the same name and tunnel. The answer carries the tunnel token the app
// runs `cloudflared` with.
//
// It also:
// - takes a heartbeat from each install every few minutes and, from a cron
//   trigger, sends the install's phone alert (ntfy) when the heartbeats stop
//   ("offline") and again when they resume;
// - serves app updates (latest.json and installers in the R2 bucket UPDATES)
//   in the format Tauri's updater expects;
// - serves known-good RDP Wrapper settings (rdpwrap.ini in UPDATES);
// - keeps problem reports an install's admin chooses to send (table reports;
//   read them with scripts/reports.ps1, never through this service).
//
// Bindings: ACCOUNT_ID, ZONE_ID, ZONE_NAME (plain text);
//           CF_API_TOKEN, PROVISION_KEY (secrets);
//           DB (D1: tables installs, reports), UPDATES (R2 bucket).

const API = "https://api.cloudflare.com/client/v4";

// Names never handed out, so installs can't take over common subdomains.
const RESERVED = new Set([
  "www", "mail", "email", "smtp", "imap", "pop", "ftp", "api", "app", "admin", "portal",
  "dashboard", "ns1", "ns2", "ns", "mx", "webmail", "cpanel", "autodiscover", "autoconfig",
  "status", "support", "help", "docs", "blog", "shop", "store", "m", "dev", "test", "staging",
]);

/** The ntfy server when an install names none (ntfy.sh rate-limits Cloudflare's shared addresses). */
const DEFAULT_NTFY = "https://ntfy.envs.net";

/** The largest problem report kept (D1 rows hold up to 2 MB). */
const MAX_REPORT_BYTES = 1024 * 1024;

/** How long without a heartbeat before an install counts as offline. */
const OFFLINE_AFTER_SECS = 15 * 60;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    // Public: RDP Wrapper settings (no secrets in them).
    if (url.pathname === "/v1/rdpwrap.ini" && request.method === "GET") {
      const obj = env.UPDATES && (await env.UPDATES.get("rdpwrap.ini"));
      if (!obj) return reply({ error: "Not found." }, 404);
      return new Response(obj.body, { headers: { "content-type": "text/plain; charset=utf-8" } });
    }
    if (!env.PROVISION_KEY || request.headers.get("x-provision-key") !== env.PROVISION_KEY) {
      return reply({ error: "Not allowed." }, 403);
    }
    try {
      if (url.pathname === "/v1/heartbeat" && request.method === "POST") return await heartbeat(request, env);
      if (url.pathname === "/v1/report" && request.method === "POST") return await saveReport(request, env);
      // The offline check on demand (normally run by the cron trigger), for testing.
      if (url.pathname === "/v1/check-offline" && request.method === "POST") return reply(await checkOffline(env));

      let m = url.pathname.match(/^\/v1\/update\/([\w-]+)\/([\w-]+)\/([\w.+-]+)$/);
      if (m && request.method === "GET") return await updateCheck(env, url, m[1], m[2], m[3]);
      m = url.pathname.match(/^\/v1\/download\/([\w.+ -]+)$/);
      if (m && request.method === "GET") return await download(env, decodeURIComponent(m[1]));
    } catch (e) {
      return reply({ error: String(e.message || e) }, 502);
    }
    if (url.pathname !== "/v1/provision" || request.method !== "POST") {
      return reply({ error: "Not found." }, 404);
    }
    let body;
    try {
      body = await request.json();
    } catch {
      return reply({ error: "Expected JSON." }, 400);
    }
    const installId = String(body.install_id || "");
    if (!/^[0-9a-f]{32}$/.test(installId)) return reply({ error: "Bad install id." }, 400);
    const port = Number(body.port || 8088);
    if (!Number.isInteger(port) || port < 1 || port > 65535) return reply({ error: "Bad port." }, 400);

    try {
      return reply(await provision(env, installId, port, String(body.name_hint || "")));
    } catch (e) {
      return reply({ error: String(e.message || e) }, 502);
    }
  },

  // Every few minutes: alert for installs whose heartbeats stopped.
  async scheduled(event, env, ctx) {
    ctx.waitUntil(checkOffline(env));
  },
};

// ---------------------------------------------------------------- heartbeats

async function heartbeat(request, env) {
  const body = await request.json().catch(() => ({}));
  const installId = String(body.install_id || "");
  if (!/^[0-9a-f]{32}$/.test(installId)) return reply({ error: "Bad install id." }, 400);
  const topic = /^[\w-]{8,64}$/.test(String(body.topic || "")) ? String(body.topic) : null;
  const server = /^https:\/\/[\w.-]+(:\d+)?$/.test(String(body.server || "")) ? String(body.server) : DEFAULT_NTFY;
  const name = String(body.name || "").slice(0, 100);
  const now = Math.floor(Date.now() / 1000);
  const before = await env.DB.prepare("SELECT down FROM installs WHERE install_id = ?").bind(installId).first();
  await env.DB.prepare(
    `INSERT INTO installs (install_id, name, topic, server, last_seen, down) VALUES (?1, ?2, ?3, ?4, ?5, 0)
     ON CONFLICT(install_id) DO UPDATE SET name = ?2, topic = ?3, server = ?4, last_seen = ?5, down = 0`,
  ).bind(installId, name, topic, server, now).run();
  if (before && before.down && topic) {
    await ntfy(server, topic, `${name || "The portal"} is back online`, "Access Portal is running and reachable again.", "white_check_mark");
  }
  return reply({ ok: true });
}

async function checkOffline(env) {
  const now = Math.floor(Date.now() / 1000);
  const report = [];
  const { results } = await env.DB.prepare(
    "SELECT install_id, name, topic, server, last_seen FROM installs WHERE down = 0 AND topic IS NOT NULL AND last_seen < ?",
  ).bind(now - OFFLINE_AFTER_SECS).all();
  for (const row of results) {
    const mins = Math.round((now - row.last_seen) / 60);
    const status = await ntfy(
      row.server || DEFAULT_NTFY,
      row.topic,
      `${row.name || "The portal"} is offline`,
      `No word from Access Portal for ${mins} minutes. The computer may be off, asleep, without internet, or the app was closed. Users can't connect until it's back.`,
      "rotating_light",
      "high",
    );
    report.push({ install: row.install_id, ntfy: status });
    if (status < 300) {
      await env.DB.prepare("UPDATE installs SET down = 1 WHERE install_id = ?").bind(row.install_id).run();
    }
  }
  return { checked_at: now, alerted: report };
}

/** Send a phone alert; returns the ntfy server's HTTP status (0 if unreachable). */
async function ntfy(server, topic, title, message, tags, priority = "default") {
  try {
    const res = await fetch(`${server}/${topic}`, {
      method: "POST",
      headers: { Title: title, Tags: tags, Priority: priority },
      body: message,
    });
    if (!res.ok) console.log(`ntfy ${res.status}: ${await res.text()}`);
    return res.status;
  } catch (e) {
    console.log(`ntfy failed: ${e}`);
    return 0;
  }
}

// ---------------------------------------------------------------- problem reports

async function saveReport(request, env) {
  const text = await request.text();
  if (text.length > MAX_REPORT_BYTES) return reply({ error: "The report is too large." }, 413);
  let body;
  try {
    body = JSON.parse(text);
  } catch {
    return reply({ error: "Expected JSON." }, 400);
  }
  const installId = String(body.install_id || "");
  if (!/^[0-9a-f]{32}$/.test(installId)) return reply({ error: "Bad install id." }, 400);
  // Short enough to read out over the phone: date, time and 4 random letters.
  const now = new Date();
  const stamp = now.toISOString().replace(/[-:]/g, "").slice(2, 13).replace("T", "-");
  const id = `${stamp}-${randomHex(2)}`;
  const system = body.system || {};
  const failing = Array.isArray(body.checks) ? body.checks.filter((c) => !c.ok).map((c) => c.id).join(",") : "";
  await env.DB.prepare(
    `INSERT INTO reports (id, install_id, name, version, os, failing, created, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)`,
  ).bind(
    id,
    installId,
    String(body.name || "").slice(0, 100),
    String(body.version || "").slice(0, 20),
    `${system.os || ""} ${system.display_version || ""} ${system.build || ""}`.trim().slice(0, 120),
    failing.slice(0, 200),
    Math.floor(now.getTime() / 1000),
    text,
  ).run();
  return reply({ id });
}

// ---------------------------------------------------------------- updates

/** "1.2.3" > "1.2.10"? Plain numeric comparison, part by part. */
function newer(a, b) {
  const pa = a.split(/[.+-]/).map(Number), pb = b.split(/[.+-]/).map(Number);
  for (let i = 0; i < 3; i++) {
    if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) > (pb[i] || 0);
  }
  return false;
}

async function updateCheck(env, url, target, arch, current) {
  const obj = await env.UPDATES.get("latest.json");
  if (!obj) return new Response(null, { status: 204 });
  const latest = await obj.json();
  const platform = latest.platforms?.[`${target}-${arch}`];
  if (!platform || !newer(latest.version, current)) return new Response(null, { status: 204 });
  return reply({
    version: latest.version,
    notes: latest.notes || "",
    pub_date: latest.pub_date,
    url: `${url.origin}/v1/download/${encodeURIComponent(platform.file)}`,
    signature: platform.signature,
  });
}

async function download(env, file) {
  const obj = await env.UPDATES.get(`releases/${file}`);
  if (!obj) return reply({ error: "Not found." }, 404);
  return new Response(obj.body, {
    headers: {
      "content-type": "application/octet-stream",
      "content-length": String(obj.size),
      "content-disposition": `attachment; filename="${file}"`,
    },
  });
}

async function provision(env, installId, port, nameHint) {
  const account = `/accounts/${env.ACCOUNT_ID}`;
  const tunnelName = `access-portal-${installId}`;

  // The install's tunnel, made on its first call.
  let [tunnel] = await cf(env, "GET", `${account}/cfd_tunnel?is_deleted=false&name=${tunnelName}`);
  if (!tunnel) {
    tunnel = await cf(env, "POST", `${account}/cfd_tunnel`, { name: tunnelName, config_src: "cloudflare" });
  }

  // Its public name: kept in the tunnel's routing, picked once.
  const config = await cf(env, "GET", `${account}/cfd_tunnel/${tunnel.id}/configurations`);
  let hostname = config?.config?.ingress?.find((rule) => rule.hostname)?.hostname;
  if (!hostname) hostname = await freeName(env, nameHint);

  // (Re)write the routing, so a changed local port is picked up.
  await cf(env, "PUT", `${account}/cfd_tunnel/${tunnel.id}/configurations`, {
    config: {
      ingress: [
        { hostname, service: `http://localhost:${port}` },
        { service: "http_status:404" },
      ],
    },
  });

  // The DNS record pointing the name at the tunnel (recreated if it went missing).
  const target = `${tunnel.id}.cfargotunnel.com`;
  const existing = await cf(env, "GET", `/zones/${env.ZONE_ID}/dns_records?name=${hostname}`);
  if (!existing.some((r) => r.type === "CNAME" && r.content === target)) {
    if (existing.length) throw new Error(`${hostname} is already used by another DNS record.`);
    await cf(env, "POST", `/zones/${env.ZONE_ID}/dns_records`, {
      type: "CNAME",
      name: hostname,
      content: target,
      proxied: true,
      comment: `Access Portal install ${installId}`,
    });
  }

  const token = await cf(env, "GET", `${account}/cfd_tunnel/${tunnel.id}/token`);
  return { hostname, url: `https://${hostname}`, token };
}

/** A free name under the zone, from the install's hint (its computer name). */
async function freeName(env, hint) {
  let base = hint.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 30).replace(/-+$/, "");
  if (!base || RESERVED.has(base)) base = "portal";
  for (let i = 0; i < 8; i++) {
    const label = i === 0 && !RESERVED.has(base) && base !== "portal" ? base : `${base}-${randomHex(2)}`;
    const name = `${label}.${env.ZONE_NAME}`;
    const taken = await cf(env, "GET", `/zones/${env.ZONE_ID}/dns_records?name=${name}`);
    if (!taken.length) return name;
  }
  throw new Error("Could not find a free name.");
}

function randomHex(bytes) {
  return [...crypto.getRandomValues(new Uint8Array(bytes))].map((b) => b.toString(16).padStart(2, "0")).join("");
}

async function cf(env, method, path, body) {
  const res = await fetch(API + path, {
    method,
    headers: { Authorization: `Bearer ${env.CF_API_TOKEN}`, "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const data = await res.json().catch(() => ({}));
  if (!res.ok || !data.success) {
    const why = (data.errors || []).map((e) => e.message).join("; ") || res.status;
    throw new Error(`Cloudflare ${method} ${path.split("?")[0]}: ${why}`);
  }
  return data.result;
}

function reply(data, status = 200) {
  return new Response(JSON.stringify(data), { status, headers: { "content-type": "application/json" } });
}
