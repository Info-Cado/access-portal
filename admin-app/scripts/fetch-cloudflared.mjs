// Downloads Cloudflare's tunnel program (cloudflared) that the app ships with,
// once, into src-tauri/binaries/, and checks it against the pinned checksum.
// Runs before every build; does nothing when the file is already there.

import { createHash } from "node:crypto"
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs"
import path from "node:path"
import { fileURLToPath } from "node:url"

const VERSION = "2026.10.7"
const SHA256 = "2493dbc168923bb3e59e5e7716f71deced92992eea9acd6e9d74f84ac7751343"
const URL = `https://github.com/cloudflare/cloudflared/releases/download/${VERSION}/cloudflared-windows-amd64.exe`

const here = path.dirname(fileURLToPath(import.meta.url))
const dir = path.join(here, "..", "src-tauri", "binaries")
// Tauri looks for sidecars named with the target triple.
const target = path.join(dir, "cloudflared-x86_64-pc-windows-msvc.exe")

const sha = (bytes) => createHash("sha256").update(bytes).digest("hex")

if (existsSync(target) && sha(readFileSync(target)) === SHA256) process.exit(0)

mkdirSync(dir, { recursive: true })
console.log(`Downloading cloudflared ${VERSION}…`)
const res = await fetch(URL)
if (!res.ok) throw new Error(`download failed: ${res.status} ${res.statusText}`)
const bytes = Buffer.from(await res.arrayBuffer())
const got = sha(bytes)
if (got !== SHA256) throw new Error(`cloudflared checksum mismatch: got ${got}, expected ${SHA256}`)
writeFileSync(`${target}.part`, bytes)
renameSync(`${target}.part`, target)
console.log(`cloudflared ${VERSION} ready (${(bytes.length / 1048576).toFixed(1)} MB)`)
