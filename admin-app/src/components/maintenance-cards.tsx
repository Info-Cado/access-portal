// Settings cards for keeping the portal healthy: updates, phone alerts, backups.

import { useCallback, useEffect, useState } from "react"
import { BellRingIcon, DatabaseBackupIcon, DownloadCloudIcon } from "lucide-react"
import { toast } from "sonner"

import { ConfirmDialog, type Confirmation } from "@/components/dialogs"
import { Button } from "@/components/ui/button"
import { Card, CardAction, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Field, FieldContent, FieldDescription, FieldGroup, FieldLabel, FieldSeparator } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Switch } from "@/components/ui/switch"
import {
  api,
  errorText,
  restartApp,
  testAlert,
  updateCheck,
  updateStatus,
  type BackupInfo,
  type PortalState,
  type UpdateStatus,
} from "@/lib/api"

function ago(unix: number | null) {
  if (!unix) return "never"
  return new Date(unix * 1000).toLocaleString([], { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" })
}

export function UpdatesCard() {
  const [status, setStatus] = useState<UpdateStatus | null>(null)
  const [busy, setBusy] = useState(false)
  useEffect(() => {
    updateStatus().then(setStatus).catch(() => {})
  }, [])

  const check = async (install: boolean) => {
    setBusy(true)
    try {
      const s = await updateCheck(install)
      setStatus(s)
      if (!install) toast.success(s.available ? `Version ${s.available} is available` : "Access Portal is up to date")
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Updates</CardTitle>
        <CardDescription>
          Version {status?.current ?? "…"}
          {status?.checked_at ? ` · last checked ${ago(status.checked_at)}` : ""}
        </CardDescription>
        <CardAction>
          <Button size="sm" variant="outline" disabled={busy || !status?.enabled} onClick={() => check(false)}>
            <DownloadCloudIcon />
            Check now
          </Button>
        </CardAction>
      </CardHeader>
      <CardContent>
        <FieldGroup>
          {status?.available && (
            <div className="flex items-center gap-3 rounded-lg border border-primary/30 bg-primary/5 px-3 py-2 text-sm">
              <span className="flex-1">
                Version {status.available} is available{status.notes ? `: ${status.notes}` : "."} It is being
                installed now.
              </span>
              <Button size="sm" disabled={busy} onClick={() => check(true)}>
                Install now
              </Button>
            </div>
          )}
          {status?.error && <FieldDescription className="text-destructive">{status.error}</FieldDescription>}
          <FieldDescription>
            Checked every 2 minutes. A new version installs straight away and the app restarts into it (about 15
            seconds). People connected keep their Windows sessions; their pages reconnect by themselves.
          </FieldDescription>
        </FieldGroup>
      </CardContent>
    </Card>
  )
}

export function AlertsCard({ data, reload }: { data: PortalState; reload: () => Promise<void> }) {
  const topic = data.alerts.topic ?? null
  const server = data.alerts.server
  const [busy, setBusy] = useState(false)
  const [serverInput, setServerInput] = useState(server)
  useEffect(() => setServerInput(server), [server])
  const set = async (enabled: boolean, newServer?: string) => {
    setBusy(true)
    try {
      await api("PUT", "/api/admin/settings/alerts", { enabled, server: newServer })
      await reload()
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }
  const test = async () => {
    if (!topic) return
    setBusy(true)
    try {
      await testAlert(server, topic)
      toast.success("Test alert sent")
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }
  const link = topic ? `${server}/${topic}` : ""
  const host = server.replace(/^https:\/\//, "")

  return (
    <Card>
      <CardHeader>
        <CardTitle>Phone alerts</CardTitle>
        <CardDescription>
          A notification on your phone when this computer goes offline, the public link is down, sessions fail to
          start, Remote Desktop stops, or the disk is almost full, and again when it's fixed.
        </CardDescription>
        <CardAction>
          <Switch checked={!!topic} disabled={busy} onCheckedChange={set} aria-label="Phone alerts" />
        </CardAction>
      </CardHeader>
      {topic && (
        <CardContent>
          <FieldGroup>
            <Field>
              <FieldLabel htmlFor="ntfy-topic">Subscribe in the free ntfy app</FieldLabel>
              <div className="flex gap-2">
                <Input id="ntfy-topic" readOnly value={topic} className="font-mono" onFocus={(e) => e.target.select()} />
                <Button
                  variant="outline"
                  onClick={() => navigator.clipboard.writeText(topic).then(() => toast.success("Copied"))}
                >
                  Copy
                </Button>
              </div>
              <FieldDescription>
                Install the free ntfy app (Android or iPhone), tap +, enter this topic, turn on “Use another server” and
                enter {host}. Or open {link} in a browser. Keep the topic private: anyone with it sees the alerts.
              </FieldDescription>
            </Field>
            <Field>
              <FieldLabel htmlFor="ntfy-server">Alert server</FieldLabel>
              <div className="flex gap-2">
                <Input id="ntfy-server" value={serverInput} onChange={(e) => setServerInput(e.target.value)} spellCheck={false} />
                <Button variant="outline" disabled={busy || serverInput === server} onClick={() => set(true, serverInput)}>
                  Save
                </Button>
              </div>
              <FieldDescription>
                Any ntfy server. The default also works for the “offline” alert, which is sent from Cloudflare (ntfy.sh
                limits those).
              </FieldDescription>
            </Field>
            <Button variant="outline" className="self-start" disabled={busy} onClick={test}>
              <BellRingIcon />
              Send a test alert
            </Button>
          </FieldGroup>
        </CardContent>
      )}
    </Card>
  )
}

export function BackupsCard() {
  const [backups, setBackups] = useState<BackupInfo[] | null>(null)
  const [folder, setFolder] = useState("")
  const [copyDir, setCopyDir] = useState("")
  const [keep, setKeep] = useState(14)
  const [busy, setBusy] = useState(false)
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null)

  const load = useCallback(async () => {
    const r = await api<{ backups: BackupInfo[]; settings: { copy_dir: string; keep: number }; folder: string }>(
      "GET",
      "/api/admin/backups",
    )
    setBackups(r.backups)
    setFolder(r.folder)
    setCopyDir(r.settings.copy_dir)
    setKeep(r.settings.keep)
  }, [])
  useEffect(() => {
    load().catch((e) => toast.error(errorText(e)))
  }, [load])

  const run = async (work: () => Promise<unknown>, done: string) => {
    setBusy(true)
    try {
      await work()
      await load()
      toast.success(done)
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const restore = (b: BackupInfo) =>
    setConfirmation({
      title: `Restore the backup from ${ago(b.at)}?`,
      description:
        "Users, apps, permissions, settings and the activity log go back to how they were then. What is there now is saved as a new backup first. The app restarts, so everyone connected is disconnected for a moment.",
      action: "Restore and restart",
      run: async () => {
        await api("POST", `/api/admin/backups/${b.name}/restore`)
        await restartApp()
      },
    })

  return (
    <Card>
      <CardHeader>
        <CardTitle>Backups</CardTitle>
        <CardDescription>
          Made every day: users, apps, permissions, settings, the activity log and the public link. Users' files aren't
          included. Kept in {folder || "the data folder"}.
        </CardDescription>
        <CardAction>
          <Button size="sm" variant="outline" disabled={busy} onClick={() => run(() => api("POST", "/api/admin/backups"), "Backup made")}>
            <DatabaseBackupIcon />
            Back up now
          </Button>
        </CardAction>
      </CardHeader>
      <CardContent>
        <FieldGroup>
          {backups && backups.length > 0 ? (
            <ul className="flex max-h-56 flex-col divide-y overflow-y-auto rounded-lg border">
              {backups.map((b, i) => (
                <li key={b.name} className="flex items-center gap-3 px-3 py-2 text-sm">
                  <span className="flex-1">
                    {ago(b.at)}
                    {i === 0 && <span className="text-muted-foreground"> · newest</span>}
                  </span>
                  <span className="text-xs text-muted-foreground">{(b.size / 1024).toFixed(0)} KB</span>
                  <Button size="sm" variant="ghost" disabled={busy} onClick={() => restore(b)}>
                    Restore
                  </Button>
                </li>
              ))}
            </ul>
          ) : (
            <FieldDescription>No backups yet. The first one is made within an hour, or now with Back up now.</FieldDescription>
          )}
          <FieldSeparator />
          <form
            className="flex flex-col gap-3"
            onSubmit={(e) => {
              e.preventDefault()
              run(() => api("PUT", "/api/admin/settings/backups", { copy_dir: copyDir, keep }), "Backup settings saved")
            }}
          >
            <Field>
              <FieldLabel htmlFor="copy-dir">Also copy backups to</FieldLabel>
              <Input
                id="copy-dir"
                placeholder="Nowhere else. Or e.g. D:\Backups, or a OneDrive or Google Drive folder"
                value={copyDir}
                onChange={(e) => setCopyDir(e.target.value)}
                spellCheck={false}
              />
              <FieldDescription>A second copy on another drive or a synced folder survives this computer failing.</FieldDescription>
            </Field>
            <Field orientation="horizontal">
              <FieldLabel htmlFor="keep" className="flex-1">
                Backups to keep
              </FieldLabel>
              <Input
                id="keep"
                type="number"
                min={1}
                max={365}
                className="w-24"
                value={keep}
                onChange={(e) => setKeep(Number(e.target.value))}
              />
            </Field>
            <Button type="submit" variant="outline" className="self-start" disabled={busy}>
              Save
            </Button>
          </form>
        </FieldGroup>
      </CardContent>
      <ConfirmDialog confirmation={confirmation} onClose={() => setConfirmation(null)} />
    </Card>
  )
}
