// Calls to the portal running inside this app. They go through the app's
// native side (`request` in src-tauri), in-process: no network, no sign-in.

import { invoke } from "@tauri-apps/api/core"

/** A session open right now. */
export type LiveSession = {
  /** Identifies the connection, for disconnecting it. */
  id: number
  app: string
  /** Unix seconds. */
  since: number
  address: string
  device: string
  link: "direct" | "relayed"
}

export type PortalUser = {
  id: number
  username: string
  is_admin: boolean
  totp: boolean
  win_username: string
  locked: boolean
  failed_count: number
  created_at: number
  app_ids: number[]
  sessions: LiveSession[]
}

export type PortalApp = {
  id: number
  key: string
  name: string
  hostname: string
  port: string
}

export type PortalState = {
  /** Where the portal accepts connections, e.g. 127.0.0.1:8088. */
  listen: string
  users: PortalUser[]
  apps: PortalApp[]
}

/** The public link (a Cloudflare Tunnel to name.<domain>). */
export type PublicLink = {
  state: "unavailable" | "setting-up" | "connecting" | "online" | "error"
  url?: string | null
  error?: string | null
}

export type ServerStatus =
  | { running: true; address: string; data_dir: string; public: PublicLink }
  | { running: false; error: string; data_dir: string; public: PublicLink }

/** Authenticator setup details; the portal hands these out once. */
export type TotpSetup = { secret: string; uri: string }

export class ApiError extends Error {
  status: number

  constructor(message: string, status = 0) {
    super(message)
    this.status = status
  }
}

export function serverStatus(): Promise<ServerStatus> {
  return invoke<ServerStatus>("status")
}

export async function api<T = unknown>(method: string, path: string, body?: unknown): Promise<T> {
  let res: { status: number; data: { error?: string } }
  try {
    res = await invoke("request", { method, path, body: body ?? null })
  } catch (e) {
    throw new ApiError(String(e))
  }
  if (res.status >= 200 && res.status < 300) return res.data as T
  throw new ApiError(res.data?.error ?? `The portal refused that (${res.status}).`, res.status)
}

/** "just now", "4 min", "2 h 5 min" since a Unix time. */
export function since(unix: number): string {
  const s = Math.max(0, Math.floor(Date.now() / 1000 - unix))
  if (s < 60) return "just now"
  const m = Math.floor(s / 60)
  if (m < 60) return `${m} min`
  const h = Math.floor(m / 60)
  return m % 60 ? `${h} h ${m % 60} min` : `${h} h`
}

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}
