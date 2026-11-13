import { useState, type FormEvent } from "react"
import { toast } from "sonner"

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
import { api, errorText, type PortalApp, type TotpSetup } from "@/lib/api"

const blank = {
  username: "",
  password: "",
  winUser: "",
  winPass: "",
  appIds: [] as number[],
  totp: false,
}

export function NewUserDialog({
  open,
  onOpenChange,
  apps,
  reload,
  onSecret,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  apps: PortalApp[]
  reload: () => Promise<void>
  onSecret: (secret: SecretToShow) => void
}) {
  const [form, setForm] = useState(blank)
  const [error, setError] = useState("")
  const [busy, setBusy] = useState(false)
  const set = (patch: Partial<typeof blank>) => setForm((f) => ({ ...f, ...patch }))

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
    setBusy(true)
    setError("")
    try {
      const res = await api<{ totp: TotpSetup | null }>("POST", "/api/admin/users", {
        username: form.username,
        password: form.password,
        win_username: form.winUser,
        win_password: form.winPass,
        enable_totp: form.totp,
        app_ids: form.appIds,
      })
      await reload()
      const username = form.username.trim()
      setForm(blank)
      onOpenChange(false)
      if (res.totp) onSecret({ username, totp: res.totp })
      else toast.success(`${username} created`)
    } catch (e) {
      setError(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={close}>
      <DialogContent className="max-h-[calc(100svh-2rem)] overflow-y-auto sm:max-w-lg">
        <form onSubmit={submit} autoComplete="off" className="contents">
          <DialogHeader>
            <DialogTitle>New user</DialogTitle>
            <DialogDescription>
              A portal account, the Windows account its sessions run as, and what it may open.
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
              <FieldLegend variant="label">Windows login on the RDP host</FieldLegend>
              <div className="grid gap-4 sm:grid-cols-2">
                <Field>
                  <FieldLabel htmlFor="nu-win-user">Windows username</FieldLabel>
                  <Input
                    id="nu-win-user"
                    value={form.winUser}
                    onChange={(e) => set({ winUser: e.target.value })}
                    placeholder={form.username.trim() || "Same as username"}
                    spellCheck={false}
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor="nu-win-pass">Windows password</FieldLabel>
                  <Input
                    id="nu-win-pass"
                    type="password"
                    autoComplete="new-password"
                    value={form.winPass}
                    onChange={(e) => set({ winPass: e.target.value })}
                    placeholder="Can be set later"
                  />
                </Field>
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
            <Button type="submit" disabled={busy}>
              {busy ? "Creating…" : "Create user"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
