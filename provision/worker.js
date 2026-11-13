// Access Portal sign-up service (a Cloudflare Worker).
//
// Each Access Portal install calls this once (and again on every start) with
// its install id. The first call creates a Cloudflare Tunnel for the install, a
// public name under ZONE_NAME (e.g. khush-laptop.infocado.in) pointing at it,
// and routes that name to the portal on the install's machine. Later calls
// return the same name and tunnel. The answer carries the tunnel token the app
// runs `cloudflared` with.
//
// Bindings: ACCOUNT_ID, ZONE_ID, ZONE_NAME (plain text);
//           CF_API_TOKEN, PROVISION_KEY (secrets).

const API = "https://api.cloudflare.com/client/v4";

// Names never handed out, so installs can't take over common subdomains.
const RESERVED = new Set([
  "www", "mail", "email", "smtp", "imap", "pop", "ftp", "api", "app", "admin", "portal",
  "dashboard", "ns1", "ns2", "ns", "mx", "webmail", "cpanel", "autodiscover", "autoconfig",
  "status", "support", "help", "docs", "blog", "shop", "store", "m", "dev", "test", "staging",
]);

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (url.pathname !== "/v1/provision" || request.method !== "POST") {
      return reply({ error: "Not found." }, 404);
    }
    if (!env.PROVISION_KEY || request.headers.get("x-provision-key") !== env.PROVISION_KEY) {
      return reply({ error: "Not allowed." }, 403);
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
};

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
