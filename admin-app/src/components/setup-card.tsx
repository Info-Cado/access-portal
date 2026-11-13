import { useCallback, useEffect, useState } from "react"
import { CircleAlertIcon, CircleCheckIcon, LoaderCircleIcon, WrenchIcon } from "lucide-react"
import { toast } from "sonner"

import { ConfirmDialog, type Confirmation } from "@/components/dialogs"
import { Button } from "@/components/ui/button"
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Skeleton } from "@/components/ui/skeleton"
import { api, errorText } from "@/lib/api"

export type SetupCheck = {
  id: string
  label: string
  ok: boolean
  detail: string
  /** What fixing it does; empty when the app can't fix it. */
  fix: string
  /** Fixing restarts Remote Desktop (everyone connected is disconnected). */
  restarts: boolean
  error?: string
}

/** Checks (and fixes) what this computer needs to serve portal sessions. */
export function SetupCard({ onChecked }: { onChecked?: (checks: SetupCheck[]) => void }) {
  const [checks, setChecks] = useState<SetupCheck[] | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null)

  const load = useCallback(async () => {
    const r = await api<{ checks: SetupCheck[] }>("GET", "/api/admin/setup")
    setChecks(r.checks)
    onChecked?.(r.checks)
  }, [onChecked])

  useEffect(() => {
    load().catch((e) => toast.error(errorText(e)))
  }, [load])

  const run = async (ids: string[]) => {
    setBusy(ids.join(","))
    try {
      const r = await api<{ checks: SetupCheck[] }>("POST", "/api/admin/setup", { fix: ids })
      setChecks(r.checks)
      onChecked?.(r.checks)
      const failed = r.checks.filter((c) => ids.includes(c.id) && (c.error || !c.ok))
      if (failed.length) toast.error(failed.map((c) => `${c.label}: ${c.error ?? c.detail}`).join("\n"))
      else toast.success("Done")
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(null)
    }
  }

  const fix = (ids: string[]) => {
    const restarting = checks?.filter((c) => ids.includes(c.id) && c.restarts) ?? []
    if (restarting.length === 0) return run(ids)
    setConfirmation({
      title: "Restart Remote Desktop?",
      description:
        "Fixing this restarts Remote Desktop on this computer. Everyone connected right now is disconnected for a moment; their apps keep running and their pages reconnect.",
      action: "Fix and restart",
      run: () => run(ids),
    })
  }

  const fixable = checks?.filter((c) => !c.ok && c.fix) ?? []

  return (
    <Card>
      <CardHeader>
        <CardTitle>This computer</CardTitle>
        <CardDescription>What it needs to serve portal sessions. Everything here can be set up with one click.</CardDescription>
        <CardAction>
          {fixable.length > 1 && (
            <Button size="sm" disabled={busy !== null} onClick={() => fix(fixable.map((c) => c.id))}>
              <WrenchIcon />
              Fix everything
            </Button>
          )}
        </CardAction>
      </CardHeader>
      <CardContent>
        {checks === null ? (
          <div className="flex flex-col gap-2">
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
            <Skeleton className="h-10 w-full" />
          </div>
        ) : (
          <ul className="flex flex-col divide-y">
            {checks.map((c) => (
              <li key={c.id} className="flex items-start gap-3 py-2.5 first:pt-0 last:pb-0">
                {busy?.split(",").includes(c.id) ? (
                  <LoaderCircleIcon className="mt-0.5 size-4 shrink-0 animate-spin text-muted-foreground" />
                ) : c.ok ? (
                  <CircleCheckIcon className="mt-0.5 size-4 shrink-0 text-emerald-600 dark:text-emerald-400" />
                ) : (
                  <CircleAlertIcon className="mt-0.5 size-4 shrink-0 text-amber-600 dark:text-amber-400" />
                )}
                <div className="min-w-0 flex-1 text-sm">
                  <div className="font-medium">{c.label}</div>
                  <div className="text-muted-foreground">{c.detail}</div>
                  {c.error && <div className="text-destructive">{c.error}</div>}
                  {!c.ok && c.fix && <div className="mt-0.5 text-xs text-muted-foreground">Fix: {c.fix}</div>}
                </div>
                {!c.ok && c.fix && (
                  <Button size="sm" variant="outline" disabled={busy !== null} onClick={() => fix([c.id])}>
                    Fix
                  </Button>
                )}
              </li>
            ))}
          </ul>
        )}
      </CardContent>
      <ConfirmDialog confirmation={confirmation} onClose={() => setConfirmation(null)} />
    </Card>
  )
}
