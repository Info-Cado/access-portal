import { useState } from "react"
import { CopyIcon } from "lucide-react"
import { toast } from "sonner"

import type { ViewProps } from "@/components/app-shell"
import { AlertsCard, BackupsCard, UpdatesCard } from "@/components/maintenance-cards"
import { SetupCard } from "@/components/setup-card"
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
import { api, errorText, type NetworkReach, type SessionSettings } from "@/lib/api"

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
      <SetupCard />
      <NetworkCard network={data.network} reload={reload} />
      <SessionsCard settings={data.settings} reload={reload} />
      <AlertsCard data={data} reload={reload} />
      <BackupsCard />
      <UpdatesCard data={data} reload={reload} />
      <AdminCard username={data.admin.username} isDefault={data.admin.default_password} reload={reload} />
    </div>
  )
}

/** Opening the portal straight from the local network or Tailscale, besides the public link. */
function NetworkCard({ network, reload }: { network: NetworkReach; reload: () => Promise<void> }) {
  const [busy, setBusy] = useState(false)

  const save = async (next: { lan: boolean; tailscale: boolean }, done: string) => {
    setBusy(true)
    try {
      await api("PUT", "/api/admin/settings/network", next)
      await reload()
      toast.success(done)
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const copy = (url: string) =>
    navigator.clipboard
      .writeText(url)
      .then(() => toast.success("Address copied", { description: url }))
      .catch(() => toast.error("Could not copy. Select the address and copy it by hand."))

  const lan = network.urls.filter((u) => u.kind === "lan")
  const tailscale = network.urls.filter((u) => u.kind === "tailscale")

  return (
    <Card>
      <CardHeader>
        <CardTitle>Without the internet</CardTitle>
        <CardDescription>
          Besides the public link, people can open the portal straight from this computer’s address. They sign in the
          same way, and every user’s sign-in rules still apply.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <FieldGroup>
          <Field orientation="horizontal">
            <FieldContent>
              <FieldLabel htmlFor="reach-lan">Local network</FieldLabel>
              <FieldDescription>
                {network.lan
                  ? "On. Devices on the same Wi-Fi or office network can use the address below."
                  : "Off. Devices on the same network have to use the public link."}
              </FieldDescription>
              {network.lan && <Addresses urls={lan} copy={copy} empty="This computer isn't on a local network right now." />}
            </FieldContent>
            <Switch
              id="reach-lan"
              checked={network.lan}
              disabled={busy}
              onCheckedChange={(on) =>
                save({ lan: on, tailscale: network.tailscale }, on ? "Local network access on" : "Local network access off")
              }
            />
          </Field>
          <FieldSeparator />
          <Field orientation="horizontal">
            <FieldContent>
              <FieldLabel htmlFor="reach-tailscale">Tailscale</FieldLabel>
              <FieldDescription>
                {network.tailscale
                  ? "On. Devices signed in to the same Tailscale network can use the address below, from anywhere."
                  : "Off. Tailscale devices have to use the public link."}
              </FieldDescription>
              {network.tailscale && (
                <Addresses urls={tailscale} copy={copy} empty="Tailscale isn't running on this computer." />
              )}
            </FieldContent>
            <Switch
              id="reach-tailscale"
              checked={network.tailscale}
              disabled={busy}
              onCheckedChange={(on) =>
                save({ lan: network.lan, tailscale: on }, on ? "Tailscale access on" : "Tailscale access off")
              }
            />
          </Field>
          {(network.lan || network.tailscale) && (
            <FieldDescription>
              These addresses are <code>https</code> with the portal’s own certificate, so sessions run at the same
              quality as the public link. The first time a device opens one, the browser shows a one-time “your
              connection isn’t private” warning — click <strong>Advanced → Continue</strong>; it’s remembered after
              that.
            </FieldDescription>
          )}
        </FieldGroup>
      </CardContent>
    </Card>
  )
}

function Addresses({
  urls,
  copy,
  empty,
}: {
  urls: { url: string }[]
  copy: (url: string) => void
  empty: string
}) {
  if (urls.length === 0) return <FieldDescription>{empty}</FieldDescription>
  return (
    <div className="mt-1 flex flex-wrap gap-1.5">
      {urls.map((u) => (
        <Button key={u.url} variant="outline" size="sm" className="font-mono text-xs" onClick={() => copy(u.url)}>
          <CopyIcon />
          {u.url.replace(/^http:\/\//, "")}
        </Button>
      ))}
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
                  : "Off. View screen and Take control start straight away, with nothing shown to the user."}
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
