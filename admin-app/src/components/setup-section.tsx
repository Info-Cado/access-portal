import { useState } from "react"
import { CircleAlertIcon, CircleCheckIcon, LoaderCircleIcon } from "lucide-react"
import { toast } from "sonner"

import type { SetupState } from "@/components/app-shell"
import { ConfirmDialog, type Confirmation } from "@/components/dialogs"
import { Button } from "@/components/ui/button"
import { SettingsGroup } from "@/components/settings-ui"
import { Skeleton } from "@/components/ui/skeleton"
import { api, errorText, restartAsAdmin, sendReport } from "@/lib/api"

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

/** The check a fix id works on: "wrapper-repair" repairs "wrapper". */
const checkOf = (id: string) => id.replace(/-repair$/, "")

/** Checks (and fixes) what this computer needs to serve portal sessions. The
 * checks themselves are made once by the app shell and shared with its banner. */
export function ComputerSection({ setup }: { setup: SetupState }) {
  const { checks, checking, error } = setup
  const [busy, setBusy] = useState<string | null>(null)
  const [step, setStep] = useState("")
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null)

  const run = async (ids: string[], onStep?: (text: string) => void) => {
    setBusy(ids.join(","))
    setStep("")
    try {
      const r = await api<{ checks: SetupCheck[] }>(
        "POST",
        "/api/admin/setup",
        { fix: ids },
        {
          onStep: (text) => {
            setStep(text)
            onStep?.(text)
          },
        },
      )
      setup.setChecks(r.checks)
      const failed = r.checks.filter((c) => ids.map(checkOf).includes(c.id) && (c.error || !c.ok))
      if (failed.length) toast.error(failed.map((c) => `${c.label}: ${c.error ?? c.detail}`).join("\n"))
      else toast.success("Done")
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(null)
      setStep("")
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
      run: (_, onStep) => run(ids, onStep),
    })
  }

  // Reinstall RDP Wrapper from scratch, even when it looks fine (e.g. an antivirus removed part of it).
  const repairWrapper = () =>
    setConfirmation({
      title: "Repair RDP Wrapper?",
      description:
        "This downloads RDP Wrapper again and reinstalls it, adding antivirus exclusions for it first, then restarts Remote Desktop. Everyone connected right now is disconnected for a moment; their apps keep running and their pages reconnect.",
      action: "Repair and restart",
      run: (_, onStep) => run(["wrapper-repair"], onStep),
    })

  const report = () =>
    setConfirmation({
      title: "Send a problem report?",
      description:
        "Sends Access Portal support these checks, this computer's Windows version, Remote Desktop and antivirus settings, the app's recent actions and the end of its log (which names users and the addresses they connected from). Passwords are never in it. Nothing is sent unless you click Send.",
      action: "Send",
      run: async () => {
        try {
          const id = await sendReport()
          toast.success(`Report sent. Its number is ${id}: mention it when you get in touch.`, { duration: 60_000 })
        } catch (e) {
          toast.error(errorText(e))
        }
      },
    })

  const fixable = checks?.filter((c) => !c.ok && c.fix) ?? []
  // The step of a running fix is shown on the first check it works on.
  const busyIds = busy?.split(",").map(checkOf) ?? []
  const stepOn = checks?.find((c) => busyIds.includes(c.id))?.id

  const problems = checks?.filter((c) => !c.ok).length ?? 0
  const summary =
    checks === null ? (checking ? "Checking…" : "") : problems === 0 ? "Ready for portal sessions" : `${problems} to fix`

  return (
    <>
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <span className="mr-auto text-sm text-muted-foreground">{summary}</span>
        {fixable.length > 1 && (
          <Button size="sm" disabled={busy !== null} onClick={() => fix(fixable.map((c) => c.id))}>
            Fix all
          </Button>
        )}
        <Button size="sm" variant="outline" disabled={busy !== null || checking} onClick={() => void setup.check()}>
          Check again
        </Button>
        <Button size="sm" variant="outline" disabled={busy !== null || checks === null} onClick={report}>
          Send report
        </Button>
      </div>
      {error && <p className="mb-3 text-sm text-destructive">{error}</p>}
      {checks === null ? (
        checking && (
          <div className="flex flex-col gap-2">
            <Skeleton className="h-14 w-full" />
            <Skeleton className="h-14 w-full" />
            <Skeleton className="h-14 w-full" />
          </div>
        )
      ) : (
        <SettingsGroup>
          {checks.map((c) => (
            <div key={c.id} className="flex min-h-14 items-center gap-3 px-4 py-3">
              {busyIds.includes(c.id) ? (
                <LoaderCircleIcon className="size-4 shrink-0 animate-spin text-muted-foreground" />
              ) : c.ok ? (
                <CircleCheckIcon className="size-4 shrink-0 text-emerald-600 dark:text-emerald-400" />
              ) : (
                <CircleAlertIcon className="size-4 shrink-0 text-amber-600 dark:text-amber-400" />
              )}
              <div className="min-w-0 flex-1">
                <div className="text-sm">{c.label}</div>
                <div className="mt-0.5 text-xs text-muted-foreground">
                  {c.id === stepOn && step ? step : !c.ok && c.fix ? `${c.detail} · Fix: ${c.fix}` : c.detail}
                </div>
                {c.error && <div className="mt-0.5 text-xs text-destructive">{c.error}</div>}
              </div>
              {!c.ok && c.fix && (
                <Button size="sm" variant="outline" disabled={busy !== null} onClick={() => fix([c.id])}>
                  Fix
                </Button>
              )}
              {c.id === "wrapper" && (
                <Button size="sm" variant="ghost" disabled={busy !== null} onClick={repairWrapper}>
                  Repair
                </Button>
              )}
              {!c.ok && c.id === "admin" && (
                <Button size="sm" variant="outline" onClick={() => restartAsAdmin().catch((e) => toast.error(errorText(e)))}>
                  Restart as administrator
                </Button>
              )}
            </div>
          ))}
        </SettingsGroup>
      )}
      <ConfirmDialog confirmation={confirmation} onClose={() => setConfirmation(null)} />
    </>
  )
}
