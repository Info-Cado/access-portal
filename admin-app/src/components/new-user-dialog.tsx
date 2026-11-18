import { useEffect, useState, type FormEvent } from "react"
import { toast } from "sonner"

import { BusyLabel } from "@/components/busy"
import type { SecretToShow } from "@/components/dialogs"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldError,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSeparator,
  FieldSet,
} from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { refreshWindowsAccounts } from "@/components/windows-accounts"
import { useAction } from "@/hooks/use-action"
import { api, errorText, type PortalApp, type TotpSetup } from "@/lib/api"

const blank = {
  username: "",
  password: "",
  winUser: "",
  winPass: "",
  appIds: [] as number[],
  totp: false,
  createWin: true,
}

/** Start the dialog with a Windows account chosen, to add another portal login
 * that signs in as it (several portal users can share one Windows account). */
export type NewUserPreset = { winUser: string; accountName: string }

export function NewUserDialog({
  open,
  onOpenChange,
  apps,
  reload,
  onSecret,
  preset,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  apps: PortalApp[]
  reload: () => Promise<void>
  onSecret: (secret: SecretToShow) => void
  preset?: NewUserPreset | null
}) {
  const [form, setForm] = useState(blank)
  const [error, setError] = useState("")
  const { busy, step, run } = useAction()
  const set = (patch: Partial<typeof blank>) => setForm((f) => ({ ...f, ...patch }))

  // Opening with a preset ties the new login to an existing Windows account and
  // needs its password, so "create a Windows account" is off and locked.
  const locked = !!preset
  useEffect(() => {
    if (open) {
      setForm(preset ? { ...blank, winUser: preset.winUser, createWin: false } : blank)
      setError("")
    }
  }, [open, preset])

  const close = (next: boolean) => {
    if (busy) return
    if (!next) {
      setForm(blank)
      setError("")
    }
    onOpenChange(next)
  }

  const submit = async (ev: FormEvent) => {
    ev.preventDefault()
    setError("")
    try {
      const res = await run((onStep) =>
        api<{ totp: TotpSetup | null }>(
          "POST",
          "/api/admin/users",
          {
            username: form.username,
            password: form.password,
            win_username: form.winUser,
            win_password: form.createWin ? "" : form.winPass,
            create_windows_account: form.createWin,
            enable_totp: form.totp,
            app_ids: form.appIds,
          },
          { onStep },
        ),
      )
      const username = form.username.trim()
      setForm(blank)
      onOpenChange(false)
      if (res.totp) onSecret({ username, totp: res.totp })
      else toast.success(`${username} created`)
      reload().catch(() => {})
      void refreshWindowsAccounts()
    } catch (e) {
      setError(errorText(e))
    }
  }

  return (
    <Dialog open={open} onOpenChange={close}>
      <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-lg">
        <form onSubmit={submit} autoComplete="off" className="contents">
          <DialogHeader>
            <DialogTitle>{locked ? `New portal login for ${preset?.accountName}` : "New user"}</DialogTitle>
            <DialogDescription>
              {locked
                ? "Another portal account that signs in as this Windows account. Give it its own portal username and password."
                : "A portal account, the Windows account its sessions run as, and what it may open."}
            </DialogDescription>
          </DialogHeader>

          <FieldGroup>
            <div className="grid gap-4 sm:grid-cols-2">
              <Field>
                <FieldLabel htmlFor="nu-username">Username</FieldLabel>
                <Input
                  id="nu-username"
                  value={form.username}
                  onChange={(e) => set({ username: e.target.value })}
                  spellCheck={false}
                  required
                />
              </Field>
              <Field>
                <FieldLabel htmlFor="nu-password">Portal password</FieldLabel>
                <Input
                  id="nu-password"
                  type="password"
                  autoComplete="new-password"
                  value={form.password}
                  onChange={(e) => set({ password: e.target.value })}
                  required
                />
              </Field>
            </div>

            <FieldSeparator />

            <FieldSet>
              <FieldLegend variant="label">Windows account on this computer</FieldLegend>
              {!locked && (
                <Field orientation="horizontal">
                  <FieldContent>
                    <FieldLabel htmlFor="nu-create-win">Create a Windows account for them</FieldLabel>
                    <FieldDescription>
                      {form.createWin
                        ? "A new account with a random password nobody needs to know, restricted to the apps you assign."
                        : "Use an account that already exists on this computer."}
                    </FieldDescription>
                  </FieldContent>
                  <Switch id="nu-create-win" checked={form.createWin} onCheckedChange={(createWin) => set({ createWin })} />
                </Field>
              )}
              <div className="grid gap-4 sm:grid-cols-2">
                <Field>
                  <FieldLabel htmlFor="nu-win-user">Windows username</FieldLabel>
                  <Input
                    id="nu-win-user"
                    value={form.winUser}
                    onChange={(e) => set({ winUser: e.target.value })}
                    placeholder={form.username.trim() || "Same as username"}
                    spellCheck={false}
                    readOnly={locked}
                  />
                </Field>
                {!form.createWin && (
                  <Field>
                    <FieldLabel htmlFor="nu-win-pass">Windows password</FieldLabel>
                    <Input
                      id="nu-win-pass"
                      type="password"
                      autoComplete="new-password"
                      value={form.winPass}
                      onChange={(e) => set({ winPass: e.target.value })}
                      required
                    />
                  </Field>
                )}
              </div>
            </FieldSet>

            {apps.length > 0 && (
              <>
                <FieldSeparator />
                <FieldSet>
                  <FieldLegend variant="label">Apps</FieldLegend>
                  <div className="grid gap-3 sm:grid-cols-2">
                    {apps.map((app) => (
                      <Field key={app.id} orientation="horizontal">
                        <Checkbox
                          id={`nu-app-${app.id}`}
                          checked={form.appIds.includes(app.id)}
                          onCheckedChange={(checked) =>
                            set({
                              appIds:
                                checked === true
                                  ? [...form.appIds, app.id]
                                  : form.appIds.filter((id) => id !== app.id),
                            })
                          }
                        />
                        <FieldLabel htmlFor={`nu-app-${app.id}`} className="font-normal">
                          {app.name}
                        </FieldLabel>
                      </Field>
                    ))}
                  </div>
                </FieldSet>
              </>
            )}

            <FieldSeparator />

            <Field orientation="horizontal">
              <FieldContent>
                <FieldLabel htmlFor="nu-totp">Two-step verification</FieldLabel>
                <FieldDescription>Ask for an authenticator code at every sign-in.</FieldDescription>
              </FieldContent>
              <Switch id="nu-totp" checked={form.totp} onCheckedChange={(totp) => set({ totp })} />
            </Field>

            {error && <FieldError>{error}</FieldError>}
          </FieldGroup>

          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => close(false)} disabled={busy}>
              Cancel
            </Button>
            <Button type="submit" className="max-w-full min-w-0" disabled={busy}>
              {busy ? <BusyLabel step={step} fallback="Creating…" /> : "Create user"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
