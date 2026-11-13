// Calls to the portal running inside this app. They go through the app's
// native side (`request` in src-tauri), in-process: no network, no sign-in.

import { invoke } from "@tauri-apps/api/core"

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

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}
