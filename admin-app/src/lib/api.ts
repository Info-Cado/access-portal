// Calls to the portal. Everything goes through the app's native side
// (`request` in src-tauri), which holds the session cookie.

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
  me: { id: number; username: string }
  users: PortalUser[]
  apps: PortalApp[]
}

/** Authenticator setup details; the portal hands these out once. */
export type TotpSetup = { secret: string; uri: string }

export class ApiError extends Error {
  status: number
  /** The portal wants a two-step verification code. */
  needCode: boolean

  constructor(message: string, status = 0, needCode = false) {
    super(message)
    this.status = status
    this.needCode = needCode
  }
}

let onSignedOut: (() => void) | undefined

/** Called when the portal says the session is gone, so the app can show sign-in. */
export function setSignedOutHandler(handler: () => void) {
  onSignedOut = handler
}

/** Choose the portal to talk to. Resolves with the cleaned-up address. */
export async function setServer(url: string): Promise<string> {
  try {
    return await invoke<string>("set_server", { url })
  } catch (e) {
    throw new ApiError(String(e))
  }
}

export async function api<T = unknown>(method: string, path: string, body?: unknown): Promise<T> {
  let res: { status: number; data: { error?: string; need_code?: boolean } }
  try {
    res = await invoke("request", { method, path, body: body ?? null })
  } catch (e) {
    throw new ApiError(String(e))
  }
  if (res.status >= 200 && res.status < 300) return res.data as T
  if (res.status === 401 && path !== "/api/login") onSignedOut?.()
  throw new ApiError(
    res.data?.error ?? `The portal refused that (${res.status}).`,
    res.status,
    Boolean(res.data?.need_code),
  )
}

export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}
