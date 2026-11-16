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

/** When and from where a user may sign in; empty means any time, anywhere. */
export type AccessRules = {
  /** 0 = Sunday … 6 = Saturday; absent = every day. */
  days?: number[]
  /** "HH:MM" */
  from?: string
  to?: string
  ips?: string[]
  countries?: string[]
}

export type PortalUser = {
  id: number
  username: string
  is_admin: boolean
  totp: boolean
  win_username: string
  locked: boolean
  /** When an administrator's block ends (unix seconds), or null if not blocked. */
  blocked_until: number | null
  failed_count: number
  created_at: number
  app_ids: number[]
  sessions: LiveSession[]
  access: AccessRules
  /** "Any time, from anywhere", "On Mon–Fri, 09:00–18:00 · in IN", … */
  access_text: string
}

export type SessionSettings = {
  /** Disconnect idle browser sessions after this many minutes; 0 = never. */
  idle_minutes: number
  /** Sign out Windows sessions left disconnected this many hours; 0 = never. */
  signout_hours: number
  /** Ask the user before viewing or controlling their screen. */
  shadow_consent: boolean
}

export type ActivityEntry = {
  id: number
  /** Unix seconds. */
  at: number
  user_id: number | null
  username: string
  kind: string
  detail: string
  address: string
}

export type PortalApp = {
  id: number
  key: string
  name: string
  hostname: string
  port: string
  /** When set, the app opens just this program on a black screen (no desktop). */
  program: string | null
  /** The program's folder, and the only folder the user's account is granted. */
  work_dir: string | null
}

export type PortalState = {
  /** Where the portal accepts connections, e.g. 127.0.0.1:8088. */
  listen: string
  users: PortalUser[]
  apps: PortalApp[]
  settings: SessionSettings
  admin: { username: string; default_password: boolean }
  updates: { auto: boolean }
  alerts: { topic?: string | null; server: string }
  /** The portal user that opens the computer owner's own desktop, if any. */
  owner_user_id: number | null
  network: NetworkReach
}

/** Opening the portal directly from the local network or Tailscale. */
export type NetworkReach = {
  lan: boolean
  tailscale: boolean
  port: number
  /** Addresses people can type, for the networks that are on. */
  urls: { kind: "lan" | "tailscale"; url: string }[]
}

export type BackupInfo = { name: string; at: number; size: number }

/** A local Windows account on this computer (Windows' built-in ones are left out). */
export type WindowsAccount = {
  name: string
  full_name: string
  description: string
  enabled: boolean
  is_admin: boolean
  /** In the "Portal RDP Users" group. */
  portal: boolean
  /** The account Access Portal runs as: the computer's owner. */
  me: boolean
  /** Signs in with a Microsoft account (its password is changed online). */
  microsoft: boolean
  /** Unix seconds, or null if it never signed in. */
  last_logon: number | null
  /** Portal users whose sessions run as this account. */
  portal_users: { id: number; username: string }[]
}

export type WindowsAccounts = {
  accounts: WindowsAccount[]
  /** The portal user that opens the owner's own desktop, if turned on. */
  owner_user_id: number | null
}

export type UpdateStatus = {
  current: string
  available: string | null
  notes: string | null
  checked_at: number | null
  state: "idle" | "checking" | "installing"
  error: string | null
  enabled: boolean
}

/** The public link (a Cloudflare Tunnel to name.<domain>). */
export type PublicLink = {
  state: "unavailable" | "setting-up" | "connecting" | "online" | "error"
  url?: string | null
  error?: string | null
}

export type ServerStatus =
  | { running: true; address: string; data_dir: string; public: PublicLink; locked: boolean }
  | { running: false; error: string; data_dir: string; public: PublicLink; locked: boolean }

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
  // The app locked itself (closed to the tray, or idle): show the sign-in again.
  if (res.status === 423) window.dispatchEvent(new Event("portal-locked"))
  throw new ApiError(res.data?.error ?? `The portal refused that (${res.status}).`, res.status)
}

/** Sign in to the app. */
export async function unlock(username: string, password: string): Promise<{ default_password: boolean }> {
  try {
    return await invoke("unlock", { username, password })
  } catch (e) {
    throw new ApiError(String(e))
  }
}

export function lockApp(): Promise<void> {
  return invoke("lock")
}

export function restartApp(): Promise<void> {
  return invoke("restart_app")
}

/** Choose a program or shortcut (a shortcut stays a shortcut). Null if cancelled. */
export function pickProgram(): Promise<string | null> {
  return invoke("pick_program")
}

/** Choose a folder. Null if cancelled. */
export async function pickFolder(title: string, start?: string): Promise<string | null> {
  const { open } = await import("@tauri-apps/plugin-dialog")
  const chosen = await open({ directory: true, multiple: false, title, defaultPath: start || undefined })
  return typeof chosen === "string" ? chosen : null
}

export type Shortcut = { target: string; args: string; work_dir: string; data_folder: string | null }

/** What a shortcut runs, and the folder it opens (for a Miracle company shortcut). */
export function readShortcut(path: string): Promise<Shortcut> {
  return invoke("read_shortcut", { path })
}

export function testAlert(server: string, topic: string): Promise<void> {
  return invoke("test_alert", { server, topic })
}

export function updateStatus(): Promise<UpdateStatus> {
  return invoke("update_status")
}

/** Look for an update now; with `install`, install it (the app restarts). */
export async function updateCheck(install: boolean): Promise<UpdateStatus> {
  try {
    return await invoke("update_check", { install })
  } catch (e) {
    throw new ApiError(String(e))
  }
}

/** Save text as a file in Downloads and show it in Explorer; returns the path. */
export function saveDownload(name: string, contents: string): Promise<string> {
  return invoke("save_download", { name, contents })
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
