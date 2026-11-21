// Settings sections for keeping the portal healthy: phone alerts, backups, updates.

import { useCallback, useEffect, useState } from "react"
import { toast } from "sonner"

import { ConfirmDialog, type Confirmation } from "@/components/dialogs"
import { SettingsGroup, SettingsNote, SettingsRow } from "@/components/settings-ui"
import { Button } from "@/components/ui/button"
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

export function UpdatesSection() {
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
    <>
      <SettingsGroup>
        <SettingsRow
          title={`Version ${status?.current ?? "…"}`}
          description={`${status?.checked_at ? `Checked ${ago(status.checked_at)}. ` : ""}New versions install by themselves; sessions stay open`}
        >
          <Button size="sm" variant="outline" disabled={busy || !status?.enabled} onClick={() => check(false)}>
            Check now
          </Button>
        </SettingsRow>
        {status?.available && (
          <SettingsRow title={`Version ${status.available} is ready`} description={status.notes || undefined}>
            <Button size="sm" disabled={busy} onClick={() => check(true)}>
              Install now
            </Button>
          </SettingsRow>
        )}
      </SettingsGroup>
      {status?.error && <SettingsNote className="text-destructive">{status.error}</SettingsNote>}
    </>
  )
}

export function AlertsSection({ data, reload }: { data: PortalState; reload: () => Promise<void> }) {
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
  const host = server.replace(/^https:\/\//, "")

  return (
    <SettingsGroup>
      <SettingsRow
        title="Phone alerts"
        htmlFor="alerts"
        description="When this computer, the public link or Remote Desktop goes down or the disk is nearly full, and when it's fixed"
      >
        <Switch id="alerts" checked={!!topic} disabled={busy} onCheckedChange={(on) => void set(on)} />
      </SettingsRow>
      {topic && (
        <>
          <SettingsRow
            title="Topic"
            htmlFor="ntfy-topic"
            description={`In the free ntfy app, tap +, enter this topic and use the server ${host}. Keep it private`}
          >
            <Input
              id="ntfy-topic"
              readOnly
              value={topic}
              className="w-48 font-mono text-xs"
              onFocus={(e) => e.target.select()}
            />
            <Button
              size="sm"
              variant="outline"
              onClick={() => navigator.clipboard.writeText(topic).then(() => toast.success("Copied"))}
            >
              Copy
            </Button>
          </SettingsRow>
          <SettingsRow title="Alert server" htmlFor="ntfy-server" description="Any ntfy server">
            <Input
              id="ntfy-server"
              value={serverInput}
              onChange={(e) => setServerInput(e.target.value)}
              spellCheck={false}
              className="w-48"
            />
            <Button size="sm" variant="outline" disabled={busy || serverInput === server} onClick={() => set(true, serverInput)}>
              Save
            </Button>
          </SettingsRow>
          <SettingsRow title="Test alert">
            <Button size="sm" variant="outline" disabled={busy} onClick={test}>
              Send
            </Button>
          </SettingsRow>
        </>
      )}
    </SettingsGroup>
  )
}

export function BackupsSection() {
  const [backups, setBackups] = useState<BackupInfo[] | null>(null)
  const [folder, setFolder] = useState("")
  const [saved, setSaved] = useState({ copy_dir: "", keep: 14 })
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
    setSaved(r.settings)
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
      run: async (_, onStep) => {
        await api("POST", `/api/admin/backups/${b.name}/restore`, undefined, { onStep })
        onStep("Restarting…")
        await restartApp()
      },
    })

  const changed = copyDir !== saved.copy_dir || keep !== saved.keep

  return (
    <>
      <SettingsGroup>
        <SettingsRow
          title="Daily backup"
          description={`Users, apps, settings, activity and the public link. Kept in ${folder || "the data folder"}`}
        >
          <Button size="sm" variant="outline" disabled={busy} onClick={() => run(() => api("POST", "/api/admin/backups"), "Backup made")}>
            Back up now
          </Button>
        </SettingsRow>
      </SettingsGroup>

      <h3 className="mt-6 mb-2 px-1 text-sm font-medium">Saved backups</h3>
      {backups && backups.length > 0 ? (
        <SettingsGroup className="max-h-64 overflow-y-auto">
          {backups.map((b, i) => (
            <div key={b.name} className="flex h-11 items-center gap-3 px-4 text-sm">
              <span className="flex-1 tabular-nums">
                {ago(b.at)}
                {i === 0 && <span className="text-muted-foreground"> · newest</span>}
              </span>
              <span className="text-xs text-muted-foreground tabular-nums">{(b.size / 1024).toFixed(0)} KB</span>
              <Button size="sm" variant="ghost" disabled={busy} onClick={() => restore(b)}>
                Restore
              </Button>
            </div>
          ))}
        </SettingsGroup>
      ) : (
        <SettingsNote className="mt-0">None yet. The first is made within an hour</SettingsNote>
      )}

      <h3 className="mt-6 mb-2 px-1 text-sm font-medium">Storage</h3>
      <form
        onSubmit={(e) => {
          e.preventDefault()
          run(() => api("PUT", "/api/admin/settings/backups", { copy_dir: copyDir, keep }), "Backup settings saved")
        }}
      >
        <SettingsGroup>
          <SettingsRow title="Also copy to" htmlFor="copy-dir" description="Another drive or a OneDrive or Google Drive folder">
            <Input
              id="copy-dir"
              placeholder="D:\Backups"
              value={copyDir}
              onChange={(e) => setCopyDir(e.target.value)}
              spellCheck={false}
              className="w-56"
            />
          </SettingsRow>
          <SettingsRow title="Keep" htmlFor="keep" description="Older backups are deleted">
            <Input
              id="keep"
              type="number"
              min={1}
              max={365}
              className="w-20 tabular-nums"
              value={keep}
              onChange={(e) => setKeep(Number(e.target.value))}
            />
          </SettingsRow>
        </SettingsGroup>
        {changed && (
          <div className="mt-3 flex justify-end">
            <Button type="submit" size="sm" disabled={busy}>
              Save
            </Button>
          </div>
        )}
      </form>
      <ConfirmDialog confirmation={confirmation} onClose={() => setConfirmation(null)} />
    </>
  )
}
