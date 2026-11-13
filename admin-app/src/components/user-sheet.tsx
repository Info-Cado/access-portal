import { useEffect, useState } from "react"
import { Trash2Icon } from "lucide-react"
import { toast } from "sonner"

import { ConfirmDialog, type Confirmation, type SecretToShow } from "@/components/dialogs"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
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
  Sheet,
  SheetContent,
  SheetDescription,
  SheetFooter,
  SheetHeader,
  SheetTitle,
} from "@/components/ui/sheet"
import { Switch } from "@/components/ui/switch"
import { api, errorText, type PortalState, type PortalUser, type TotpSetup } from "@/lib/api"

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
    <Sheet open={user !== null} onOpenChange={(open) => !open && onClose()}>
      <SheetContent className="w-full gap-0 sm:max-w-md">
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
      </SheetContent>
    </Sheet>
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
  const isMe = user.id === data.me.id
  const base = `/api/admin/users/${user.id}`
  const [busy, setBusy] = useState(false)
  const [password, setPassword] = useState("")
  const [winUser, setWinUser] = useState(user.win_username)
  const [winPass, setWinPass] = useState("")
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null)

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
      <SheetHeader className="border-b">
        <SheetTitle className="flex items-center gap-2">
          {user.username}
          {user.is_admin && <Badge>Admin</Badge>}
          {user.locked && <Badge variant="destructive">Locked</Badge>}
        </SheetTitle>
        <SheetDescription>
          {user.is_admin ? "Administrator" : "User"}
          {isMe && " · this is you"}
        </SheetDescription>
      </SheetHeader>

      <div className="flex-1 overflow-y-auto p-4">
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
                {isMe
                  ? "Changes the password you sign in with."
                  : "The user is signed out of the portal everywhere."}
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

      {!isMe && (
        <SheetFooter className="border-t">
          <Button
            variant="destructive"
            disabled={busy}
            onClick={() =>
              setConfirmation({
                title: `Delete ${user.username}?`,
                description:
                  "Their portal account and app permissions are removed. The Windows account on the host is not touched.",
                action: "Delete user",
                run: async () => {
                  await api("DELETE", base)
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
        </SheetFooter>
      )}

      <ConfirmDialog confirmation={confirmation} onClose={() => setConfirmation(null)} />
    </>
  )
}
