import { useEffect, useState } from "react"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import {
  Field,
  FieldContent,
  FieldDescription,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSet,
} from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import { Textarea } from "@/components/ui/textarea"
import { api, errorText, type AccessRules, type PortalUser } from "@/lib/api"

// Monday first, as people read a week.
const DAYS: [number, string][] = [
  [1, "Mon"],
  [2, "Tue"],
  [3, "Wed"],
  [4, "Thu"],
  [5, "Fri"],
  [6, "Sat"],
  [0, "Sun"],
]

/** When and from where a user may sign in. */
export function AccessRulesEditor({ user, reload }: { user: PortalUser; reload: () => Promise<void> }) {
  const initial = user.access
  const [days, setDays] = useState<number[]>(initial.days ?? [0, 1, 2, 3, 4, 5, 6])
  const [limitHours, setLimitHours] = useState(Boolean(initial.from))
  const [from, setFrom] = useState(initial.from ?? "09:00")
  const [to, setTo] = useState(initial.to ?? "18:00")
  const [ips, setIps] = useState((initial.ips ?? []).join("\n"))
  const [countries, setCountries] = useState((initial.countries ?? []).join(", "))
  const [busy, setBusy] = useState(false)

  // Follow saves made elsewhere (and the user switching).
  useEffect(() => {
    setDays(initial.days ?? [0, 1, 2, 3, 4, 5, 6])
    setLimitHours(Boolean(initial.from))
    setFrom(initial.from ?? "09:00")
    setTo(initial.to ?? "18:00")
    setIps((initial.ips ?? []).join("\n"))
    setCountries((initial.countries ?? []).join(", "))
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [user.id, JSON.stringify(initial)])

  const rules = (): AccessRules => ({
    days: days.length === 7 ? undefined : days,
    from: limitHours ? from : undefined,
    to: limitHours ? to : undefined,
    ips: ips.split(/[\s,]+/).filter(Boolean),
    countries: countries.split(/[\s,]+/).filter(Boolean),
  })

  const save = async (next: AccessRules, done: string) => {
    setBusy(true)
    try {
      await api("PUT", `/api/admin/users/${user.id}/access`, { rules: next })
      await reload()
      toast.success(done)
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const toggleDay = (d: number) => setDays((cur) => (cur.includes(d) ? cur.filter((x) => x !== d) : [...cur, d]))

  return (
    <FieldSet>
      <FieldLegend variant="label">When and where they can sign in</FieldLegend>
      <FieldDescription>
        Now: {user.access_text}. Times are this computer's clock. A session is ended when its hours run out
        (apps keep running).
      </FieldDescription>
      <FieldGroup className="gap-4">
        <Field>
          <FieldLabel>Days</FieldLabel>
          <div className="flex flex-wrap gap-1">
            {DAYS.map(([d, label]) => (
              <Button
                key={d}
                type="button"
                size="sm"
                variant={days.includes(d) ? "default" : "outline"}
                className="w-12"
                aria-pressed={days.includes(d)}
                disabled={busy}
                onClick={() => toggleDay(d)}
              >
                {label}
              </Button>
            ))}
          </div>
        </Field>
        <Field orientation="horizontal">
          <FieldContent>
            <FieldLabel htmlFor="limit-hours">Only between certain hours</FieldLabel>
            <FieldDescription>Over midnight works too, e.g. 22:00 to 06:00.</FieldDescription>
          </FieldContent>
          <Switch id="limit-hours" checked={limitHours} disabled={busy} onCheckedChange={setLimitHours} />
        </Field>
        {limitHours && (
          <div className="flex items-center gap-2">
            <Input type="time" className="w-32" value={from} onChange={(e) => setFrom(e.target.value)} aria-label="From" />
            <span className="text-sm text-muted-foreground">to</span>
            <Input type="time" className="w-32" value={to} onChange={(e) => setTo(e.target.value)} aria-label="To" />
          </div>
        )}
        <Field>
          <FieldLabel htmlFor="ips">Only from these networks</FieldLabel>
          <Textarea
            id="ips"
            rows={2}
            spellCheck={false}
            placeholder="Any network. Or e.g. 203.0.113.7 or 203.0.113.0/24, one per line"
            value={ips}
            onChange={(e) => setIps(e.target.value)}
          />
          <FieldDescription>Public addresses, as shown under Connected now or in Activity.</FieldDescription>
        </Field>
        <Field>
          <FieldLabel htmlFor="countries">Only from these countries</FieldLabel>
          <Input
            id="countries"
            spellCheck={false}
            placeholder="Any country. Or two-letter codes, e.g. IN, AE"
            value={countries}
            onChange={(e) => setCountries(e.target.value)}
          />
        </Field>
        <div className="flex gap-2">
          <Button type="button" variant="outline" disabled={busy} onClick={() => save(rules(), "Access rules saved")}>
            Save rules
          </Button>
          {user.access_text !== "Any time, from anywhere" && (
            <Button type="button" variant="ghost" disabled={busy} onClick={() => save({}, "Limits removed")}>
              Remove limits
            </Button>
          )}
        </div>
      </FieldGroup>
    </FieldSet>
  )
}
