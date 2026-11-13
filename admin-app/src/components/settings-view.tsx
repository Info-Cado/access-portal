import { useState } from "react"
import { toast } from "sonner"

import type { ViewProps } from "@/components/app-shell"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldGroup,
  FieldLabel,
  FieldSeparator,
} from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { NativeSelect } from "@/components/ui/native-select"
import { Switch } from "@/components/ui/switch"
import { api, errorText, type SessionSettings } from "@/lib/api"

const IDLE_CHOICES: [number, string][] = [
  [0, "Never"],
  [15, "After 15 minutes"],
  [30, "After 30 minutes"],
  [60, "After 1 hour"],
  [120, "After 2 hours"],
  [240, "After 4 hours"],
  [480, "After 8 hours"],
]

const SIGNOUT_CHOICES: [number, string][] = [
  [0, "Never"],
  [1, "After 1 hour"],
  [2, "After 2 hours"],
  [4, "After 4 hours"],
  [8, "After 8 hours"],
  [12, "After 12 hours"],
  [24, "After 1 day"],
  [72, "After 3 days"],
]

/** Keep a value that isn't one of the presets selectable. */
function withCurrent(choices: [number, string][], value: number, unit: string): [number, string][] {
  return choices.some(([v]) => v === value) ? choices : [...choices, [value, `After ${value} ${unit}`]]
}

export function SettingsView({ data, reload }: ViewProps) {
  return (
    <div className="flex max-w-2xl flex-col gap-4">
      <SessionsCard settings={data.settings} reload={reload} />
      <AdminCard username={data.admin.username} isDefault={data.admin.default_password} reload={reload} />
    </div>
  )
}

function SessionsCard({ settings, reload }: { settings: SessionSettings; reload: () => Promise<void> }) {
  const [busy, setBusy] = useState(false)

  const save = async (next: SessionSettings, done: string) => {
    setBusy(true)
    try {
      await api("PUT", "/api/admin/settings/sessions", next)
      await reload()
      toast.success(done)
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Sessions</CardTitle>
        <CardDescription>Apply to every user. Changes take effect for sessions opened from now on.</CardDescription>
      </CardHeader>
      <CardContent>
        <FieldGroup>
          <Field orientation="responsive">
            <FieldContent>
              <FieldLabel htmlFor="idle">Disconnect idle sessions</FieldLabel>
              <FieldDescription>
                When nobody has touched the keyboard or mouse for this long. Their apps keep running, and
                they get a one-minute warning first.
              </FieldDescription>
            </FieldContent>
            <NativeSelect
              id="idle"
              className="sm:w-48"
              value={settings.idle_minutes}
              disabled={busy}
              onChange={(e) =>
                save({ ...settings, idle_minutes: Number(e.target.value) }, "Idle sessions setting saved")
              }
            >
              {withCurrent(IDLE_CHOICES, settings.idle_minutes, "minutes").map(([v, label]) => (
                <option key={v} value={v}>
                  {label}
                </option>
              ))}
            </NativeSelect>
          </Field>
          <FieldSeparator />
          <Field orientation="responsive">
            <FieldContent>
              <FieldLabel htmlFor="signout">Sign out disconnected Windows sessions</FieldLabel>
              <FieldDescription>
                When a user's Windows session has been left disconnected this long, sign it out. Their apps
                close and unsaved work is lost. Windows administrator accounts are never signed out.
              </FieldDescription>
            </FieldContent>
            <NativeSelect
              id="signout"
              className="sm:w-48"
              value={settings.signout_hours}
              disabled={busy}
              onChange={(e) =>
                save({ ...settings, signout_hours: Number(e.target.value) }, "Sign-out setting saved")
              }
            >
              {withCurrent(SIGNOUT_CHOICES, settings.signout_hours, "hours").map(([v, label]) => (
                <option key={v} value={v}>
                  {label}
                </option>
              ))}
            </NativeSelect>
          </Field>
          <FieldSeparator />
          <Field orientation="horizontal">
            <FieldContent>
              <FieldLabel htmlFor="consent">Ask before viewing a user's screen</FieldLabel>
              <FieldDescription>
                {settings.shadow_consent
                  ? "On. Windows asks the user to allow it each time you view or control their screen."
                  : "Off. View screen and Take control start straight away; the user is told on their screen."}
              </FieldDescription>
            </FieldContent>
            <Switch
              id="consent"
              checked={settings.shadow_consent}
              disabled={busy}
              onCheckedChange={(on) =>
                save({ ...settings, shadow_consent: on }, on ? "Users will be asked first" : "Users won't be asked")
              }
            />
          </Field>
        </FieldGroup>
      </CardContent>
    </Card>
  )
}

function AdminCard({
  username,
  isDefault,
  reload,
}: {
  username: string
  isDefault: boolean
  reload: () => Promise<void>
}) {
  const [name, setName] = useState(username)
  const [current, setCurrent] = useState("")
  const [password, setPassword] = useState("")
  const [again, setAgain] = useState("")
  const [busy, setBusy] = useState(false)
  const mismatch = again.length > 0 && password !== again

  const submit = async (ev: React.FormEvent) => {
    ev.preventDefault()
    setBusy(true)
    try {
      await api("PUT", "/api/admin/auth", { current_password: current, username: name, password })
      setCurrent("")
      setPassword("")
      setAgain("")
      await reload()
      toast.success("Sign-in changed", { description: "Use it the next time the app asks." })
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>App sign-in</CardTitle>
        <CardDescription>
          {isDefault
            ? "Still the default (admin / admin). Choose your own so only you can open this app."
            : "Asked when the app opens, after it is closed to the tray, and after 15 minutes without use."}
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form onSubmit={submit} autoComplete="off">
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="admin-name">Username</FieldLabel>
              <Input id="admin-name" value={name} onChange={(e) => setName(e.target.value)} spellCheck={false} />
            </Field>
            <Field>
              <FieldLabel htmlFor="admin-current">Current password</FieldLabel>
              <Input
                id="admin-current"
                type="password"
                autoComplete="current-password"
                value={current}
                onChange={(e) => setCurrent(e.target.value)}
              />
            </Field>
            <Field>
              <FieldLabel htmlFor="admin-new">New password</FieldLabel>
              <Input
                id="admin-new"
                type="password"
                autoComplete="new-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
              <FieldDescription>At least 8 characters.</FieldDescription>
            </Field>
            <Field data-invalid={mismatch ? true : undefined}>
              <FieldLabel htmlFor="admin-again">New password again</FieldLabel>
              <Input
                id="admin-again"
                type="password"
                autoComplete="new-password"
                value={again}
                aria-invalid={mismatch ? true : undefined}
                onChange={(e) => setAgain(e.target.value)}
              />
              {mismatch && <FieldDescription className="text-destructive">The passwords don't match.</FieldDescription>}
            </Field>
            <Button
              type="submit"
              className="self-start"
              disabled={busy || !current || password.length < 8 || password !== again || !name.trim()}
            >
              Change sign-in
            </Button>
          </FieldGroup>
        </form>
      </CardContent>
    </Card>
  )
}
