import { useEffect, useState } from "react"
import { ChevronDownIcon, EyeIcon, MessageSquareTextIcon, MousePointer2Icon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"

import { AccessRulesEditor } from "@/components/access-rules"
import { ConfirmDialog, type Confirmation, type SecretToShow } from "@/components/dialogs"
import { MessageDialog, type MessageTarget } from "@/components/message-dialog"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSeparator,
  FieldSet,
} from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Skeleton } from "@/components/ui/skeleton"
import { Switch } from "@/components/ui/switch"
import { api, errorText, since, type PortalState, type PortalUser, type TotpSetup } from "@/lib/api"

/** Blocks at or beyond this (unix seconds, ~year 3000) mean "until unblocked". */
const FAR_FUTURE = 32_503_680_000

/** How long a disconnect can block a user from reconnecting. -1 = until unblocked. */
const BLOCK_CHOICES: [number, string][] = [
  [5, "For 5 minutes"],
  [15, "For 15 minutes"],
  [60, "For 1 hour"],
  [480, "For 8 hours"],
  [-1, "Until I unblock"],
]

export function UserSheet({
  user,
  data,
  reload,
  onClose,
  onSecret,
}: {
  user: PortalUser | null
  data: PortalState
  reload: () => Promise<void>
  onClose: () => void
  onSecret: (secret: SecretToShow) => void
}) {
  // Keep showing the last user while the sheet slides shut.
  const [shown, setShown] = useState(user)
  useEffect(() => {
    if (user) setShown(user)
  }, [user])

  return (
    <Dialog open={user !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="flex max-h-[calc(100svh-3rem)] flex-col gap-0 p-0 sm:max-w-2xl">
        {shown && (
          <UserDetails
            key={shown.id}
            user={shown}
            data={data}
            reload={reload}
            onClose={onClose}
            onSecret={onSecret}
          />
        )}
      </DialogContent>
    </Dialog>
  )
}

type Mode = "app" | "full" | "missing"

/** Full Windows desktop vs restricted to assigned apps. The setting lives on the Windows
 * account itself, so it is read from the host each time the panel opens. */
function DesktopMode({ user, onMode }: { user: PortalUser; onMode: (mode: Mode | null) => void }) {
  const [mode, setModeState] = useState<Mode | null>(null)
  const setMode = (m: Mode | null) => {
    setModeState(m)
    onMode(m)
  }
  const [error, setError] = useState("")
  const [busy, setBusy] = useState(false)
  const path = `/api/admin/users/${user.id}/desktop`

  useEffect(() => {
    let live = true
    setMode(null)
    setError("")
    api<{ mode: Mode }>("GET", path)
      .then((r) => live && setMode(r.mode))
      .catch((e) => live && setError(errorText(e)))
    return () => {
      live = false
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, user.win_username])

  const createAccount = async () => {
    setBusy(true)
    try {
      const r = await api<{ mode: Mode }>("POST", `/api/admin/users/${user.id}/windows-account`)
      setMode(r.mode)
      toast.success(`Windows account ${user.win_username} created`, {
        description: "Restricted to the apps assigned to them. Its password is random and stored by the portal.",
      })
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const change = async (full: boolean) => {
    setBusy(true)
    try {
      const r = await api<{ mode: Mode }>("PUT", path, { full })
      setMode(r.mode)
      toast.success(full ? "Full desktop turned on" : "Full desktop turned off", {
        description: "Applies from their next session.",
      })
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  let description: string
  if (error) description = error
  else if (mode === null) description = "Checking the Windows account…"
  else if (mode === "missing")
    description = `There is no Windows account named ${user.win_username} on this computer yet, so this user can't open a session.`
  else if (mode === "full")
    description =
      "On. They see Desktop in the portal (the normal Windows desktop, Start menu and all programs), next to any apps assigned to them."
  else
    description =
      "Off. They can open only the apps assigned to them; with none, the portal tells them nothing is assigned. Run, command prompt, Task Manager and drive C: are blocked."

  return (
    <Field orientation="horizontal">
      <FieldContent>
        <FieldLabel htmlFor="full-desktop">Full desktop</FieldLabel>
        <FieldDescription className={error ? "text-destructive" : undefined}>
          {description}
        </FieldDescription>
        {(mode === "app" || mode === "full") && (
          <FieldDescription>Changes apply from the next session; an open one keeps its mode.</FieldDescription>
        )}
        {mode === "missing" && (
          <Button size="sm" className="mt-1 self-start" disabled={busy} onClick={createAccount}>
            Create Windows account {user.win_username}
          </Button>
        )}
      </FieldContent>
      {mode === null && !error ? (
        <Skeleton className="h-[1.15rem] w-8 rounded-full" />
      ) : (
        <Switch
          id="full-desktop"
          checked={mode === "full"}
          disabled={busy || (mode !== "app" && mode !== "full")}
          onCheckedChange={change}
        />
      )}
    </Field>
  )
}

function UserDetails({
  user,
  data,
  reload,
  onClose,
  onSecret,
}: {
  user: PortalUser
  data: PortalState
  reload: () => Promise<void>
  onClose: () => void
  onSecret: (secret: SecretToShow) => void
}) {
  const base = `/api/admin/users/${user.id}`
  const [busy, setBusy] = useState(false)
  const [password, setPassword] = useState("")
  const [winUser, setWinUser] = useState(user.win_username)
  const [winPass, setWinPass] = useState("")
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null)
  const [winMode, setWinMode] = useState<Mode | null>(null)
  const [messageTo, setMessageTo] = useState<MessageTarget | null>(null)

  /** Open Windows' viewer on their screen, on this computer. */
  const shadow = async (connectionId: number, control: boolean) => {
    setBusy(true)
    try {
      const r = await api<{ asked: boolean }>("POST", `/api/admin/connections/${connectionId}/shadow`, { control })
      toast.success(control ? "Taking control" : "Opening their screen", {
        description: r.asked
          ? "Windows is asking them to allow it. A Remote Desktop window opens here once they do."
          : "A Remote Desktop window opens on this computer. They're told on their screen. Close the window to stop.",
      })
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  /** Run one change, refresh the data, and say how it went. */
  const change = async (work: () => Promise<unknown>, done: string) => {
    setBusy(true)
    try {
      await work()
      await reload()
      if (done) toast.success(done)
      return true
    } catch (e) {
      toast.error(errorText(e))
      return false
    } finally {
      setBusy(false)
    }
  }

  const toggleApp = (appId: number, name: string, allowed: boolean) => {
    const ids = user.app_ids.filter((id) => id !== appId)
    if (allowed) ids.push(appId)
    return change(
      () => api("PUT", `${base}/apps`, { app_ids: ids }),
      allowed ? `${name} allowed` : `${name} removed`,
    )
  }

  const setTotp = async (enabled: boolean, done: string) => {
    let setup: TotpSetup | null = null
    const ok = await change(async () => {
      setup = (await api<{ totp: TotpSetup | null }>("PUT", `${base}/totp`, { enabled })).totp
    }, enabled ? "" : done)
    if (ok && setup) onSecret({ username: user.username, totp: setup })
  }

  return (
    <>
      <DialogHeader className="border-b p-5 text-left">
        <DialogTitle className="flex items-center gap-2">
          {user.username}
          {user.locked && <Badge variant="destructive">Locked</Badge>}
          {user.blocked_until && <Badge variant="destructive">Blocked</Badge>}
        </DialogTitle>
        <DialogDescription>
          Signs in as Windows account {user.win_username}
        </DialogDescription>
      </DialogHeader>

      <div className="flex-1 overflow-y-auto p-5">
        <FieldGroup>
          <FieldSet>
            <FieldLegend variant="label">Apps this user can open</FieldLegend>
            {data.apps.length === 0 ? (
              <FieldDescription>No apps exist yet. Add one under Apps.</FieldDescription>
            ) : (
              <FieldGroup className="gap-3">
                {data.apps.map((app) => (
                  <Field key={app.id} orientation="horizontal">
                    <Checkbox
                      id={`app-${app.id}`}
                      checked={user.app_ids.includes(app.id)}
                      disabled={busy}
                      onCheckedChange={(checked) => toggleApp(app.id, app.name, checked === true)}
                    />
                    <FieldLabel htmlFor={`app-${app.id}`} className="font-normal">
                      {app.name}
                    </FieldLabel>
                  </Field>
                ))}
              </FieldGroup>
            )}
          </FieldSet>

          {user.sessions.length > 0 && (
            <>
              <FieldSeparator />
              <FieldSet>
                <FieldLegend variant="label" className="flex items-center gap-2">
                  <span className="relative flex size-2">
                    <span className="absolute inline-flex size-full animate-ping rounded-full bg-emerald-500 opacity-60" />
                    <span className="relative inline-flex size-2 rounded-full bg-emerald-500" />
                  </span>
                  Connected now
                </FieldLegend>
                <div className="flex flex-col gap-2">
                  {user.sessions.map((s) => (
                    <div key={s.id} className="flex flex-col gap-2 rounded-md border px-3 py-2 text-sm">
                      <div className="min-w-0">
                        <div className="flex items-center justify-between gap-2">
                          <span className="font-medium">{s.app}</span>
                          <span className="text-xs text-muted-foreground">for {since(s.since).replace(/^just now$/, "under a minute")}</span>
                        </div>
                        <div className="text-xs text-muted-foreground">
                          {s.device} · from {s.address === "local" ? "this computer" : s.address} ·{" "}
                          {s.link === "direct" ? "direct link" : "through the portal"}
                        </div>
                      </div>
                      <div className="flex flex-wrap gap-1.5">
                        <Button variant="outline" size="sm" disabled={busy} onClick={() => shadow(s.id, false)}>
                          <EyeIcon />
                          View screen
                        </Button>
                        <Button variant="outline" size="sm" disabled={busy} onClick={() => shadow(s.id, true)}>
                          <MousePointer2Icon />
                          Take control
                        </Button>
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={busy}
                          onClick={() => setMessageTo({ label: `${user.username} (${s.app})`, connection_id: s.id })}
                        >
                          <MessageSquareTextIcon />
                          Message
                        </Button>
                        <DropdownMenu>
                          <DropdownMenuTrigger asChild>
                            <Button variant="outline" size="sm" className="ml-auto" disabled={busy}>
                              Disconnect
                              <ChevronDownIcon />
                            </Button>
                          </DropdownMenuTrigger>
                          <DropdownMenuContent align="end">
                            <DropdownMenuItem
                              onSelect={() =>
                                change(
                                  () => api("POST", `/api/admin/connections/${s.id}/disconnect`, { block_minutes: 0 }),
                                  "Disconnected",
                                )
                              }
                            >
                              Disconnect only
                            </DropdownMenuItem>
                            <DropdownMenuSeparator />
                            <DropdownMenuLabel className="text-xs font-normal text-muted-foreground">
                              Disconnect and block from reconnecting
                            </DropdownMenuLabel>
                            {BLOCK_CHOICES.map(([minutes, label]) => (
                              <DropdownMenuItem
                                key={minutes}
                                onSelect={() =>
                                  change(
                                    () =>
                                      api("POST", `/api/admin/connections/${s.id}/disconnect`, { block_minutes: minutes }),
                                    `Disconnected and blocked ${label.toLowerCase()}`,
                                  )
                                }
                              >
                                {label}
                              </DropdownMenuItem>
                            ))}
                          </DropdownMenuContent>
                        </DropdownMenu>
                      </div>
                    </div>
                  ))}
                </div>
                <FieldDescription>
                  Disconnecting leaves their apps running. Without a block they can reconnect at once; with one,
                  every session of theirs ends and they can’t sign back in until it passes. To close their apps too,
                  sign them out instead.
                </FieldDescription>
                <Button
                  variant="destructive"
                  size="sm"
                  className="self-start"
                  disabled={busy}
                  onClick={() =>
                    setConfirmation({
                      title: `Sign ${user.username} out?`,
                      description:
                        "This disconnects them, ends their portal sign-in and signs their Windows account out. Their apps close and unsaved work is lost.",
                      action: "Sign out",
                      run: async () => {
                        await api("POST", `${base}/sign-out`)
                        await reload()
                        toast.success(`${user.username} signed out`)
                      },
                    })
                  }
                >
                  Sign out of Windows
                </Button>
              </FieldSet>
            </>
          )}

          <FieldSeparator />
          <DesktopMode user={user} onMode={setWinMode} />

          <FieldSeparator />
          <AccessRulesEditor user={user} reload={reload} />

          {user.blocked_until && (
            <>
              <FieldSeparator />
              <Field orientation="horizontal">
                <FieldContent>
                  <FieldLabel>Blocked</FieldLabel>
                  <FieldDescription>
                    {user.blocked_until > FAR_FUTURE
                      ? "Blocked from the portal until you unblock them."
                      : `Blocked from the portal until ${new Date(user.blocked_until * 1000).toLocaleString()}.`}
                  </FieldDescription>
                </FieldContent>
                <Button
                  variant="outline"
                  size="sm"
                  disabled={busy}
                  onClick={() => change(() => api("POST", `${base}/unblock`), "Unblocked")}
                >
                  Unblock
                </Button>
              </Field>
            </>
          )}

          {(user.locked || user.failed_count > 0) && (
            <>
              <FieldSeparator />
              <Field orientation="horizontal">
                <FieldContent>
                  <FieldLabel>Sign-in attempts</FieldLabel>
                  <FieldDescription>
                    {user.locked
                      ? "Locked after too many wrong passwords."
                      : `${user.failed_count} wrong password${user.failed_count === 1 ? "" : "s"} so far.`}
                  </FieldDescription>
                </FieldContent>
                <Button
                  variant="outline"
                  size="sm"
                  disabled={busy}
                  onClick={() =>
                    change(
                      () => api("POST", `${base}/unlock`),
                      user.locked ? "Account unlocked" : "Counter reset",
                    )
                  }
                >
                  {user.locked ? "Unlock" : "Reset"}
                </Button>
              </Field>
            </>
          )}

          <FieldSeparator />

          <form
            autoComplete="off"
            onSubmit={async (ev) => {
              ev.preventDefault()
              if (await change(() => api("PUT", `${base}/password`, { password }), "Password changed"))
                setPassword("")
            }}
          >
            <Field>
              <FieldLabel htmlFor="new-password">Portal password</FieldLabel>
              <div className="flex gap-2">
                <Input
                  id="new-password"
                  type="password"
                  placeholder="New password"
                  autoComplete="new-password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                />
                <Button type="submit" variant="outline" disabled={busy || !password}>
                  Set password
                </Button>
              </div>
              <FieldDescription>
                The password they use on the portal’s sign-in page. Setting it signs them out
                everywhere.
              </FieldDescription>
            </Field>
          </form>

          <FieldSeparator />

          <form
            autoComplete="off"
            onSubmit={async (ev) => {
              ev.preventDefault()
              const body = { win_username: winUser, win_password: winPass }
              if (await change(() => api("PUT", `${base}/windows`, body), "Windows login saved"))
                setWinPass("")
            }}
          >
            <FieldSet>
              <FieldLegend variant="label">Windows login</FieldLegend>
              <FieldDescription>
                The account on the RDP host this user’s sessions run as. The password is stored
                encrypted and never shown again.
              </FieldDescription>
              <FieldGroup className="gap-3">
                <Field>
                  <FieldLabel htmlFor="win-user">Windows username</FieldLabel>
                  <Input
                    id="win-user"
                    value={winUser}
                    onChange={(e) => setWinUser(e.target.value)}
                    spellCheck={false}
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor="win-pass">Windows password</FieldLabel>
                  <Input
                    id="win-pass"
                    type="password"
                    autoComplete="new-password"
                    value={winPass}
                    onChange={(e) => setWinPass(e.target.value)}
                  />
                </Field>
                <Button
                  type="submit"
                  variant="outline"
                  className="self-start"
                  disabled={busy || !winUser.trim() || !winPass}
                >
                  Save Windows login
                </Button>
              </FieldGroup>
            </FieldSet>
          </form>

          <FieldSeparator />

          <Field orientation="horizontal">
            <FieldContent>
              <FieldLabel htmlFor="totp">Two-step verification</FieldLabel>
              <FieldDescription>
                {user.totp
                  ? "On. A code from their authenticator app is asked at every sign-in."
                  : "Off. Only the password is asked."}
              </FieldDescription>
              {user.totp && (
                <Button
                  variant="link"
                  size="sm"
                  className="h-auto self-start p-0"
                  disabled={busy}
                  onClick={() => setTotp(true, "")}
                >
                  Issue a new setup key
                </Button>
              )}
            </FieldContent>
            <Switch
              id="totp"
              checked={user.totp}
              disabled={busy}
              onCheckedChange={(on) => setTotp(on, "Two-step verification turned off")}
            />
          </Field>
        </FieldGroup>
      </div>

      {(
        <DialogFooter className="mx-0 mb-0 border-t sm:justify-start">
          <Button
            variant="destructive"
            disabled={busy}
            onClick={() =>
              setConfirmation({
                title: `Delete ${user.username}?`,
                description:
                  "Their portal account and app permissions are removed." +
                  (winMode === "app" || winMode === "full" ? "" : " They have no Windows account on this computer."),
                action: "Delete user",
                option:
                  winMode === "app" || winMode === "full"
                    ? { label: `Also delete the Windows account ${user.win_username} and its files on this computer`, checked: false }
                    : undefined,
                run: async (alsoWindows) => {
                  await api("DELETE", alsoWindows ? `${base}?windows=1` : base)
                  onClose()
                  await reload()
                  toast.success("User deleted")
                },
              })
            }
          >
            <Trash2Icon />
            Delete user
          </Button>
        </DialogFooter>
      )}

      <ConfirmDialog confirmation={confirmation} onClose={() => setConfirmation(null)} />
      <MessageDialog target={messageTo} onClose={() => setMessageTo(null)} />
    </>
  )
}
