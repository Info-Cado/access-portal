import { useEffect, useState, type FormEvent } from "react"
import { AppWindowIcon, FileIcon, FolderOpenIcon, PencilIcon, PlusIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"

import type { ViewProps } from "@/components/app-shell"
import { BusyLabel } from "@/components/busy"
import { ConfirmDialog, type Confirmation } from "@/components/dialogs"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import {
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from "@/components/ui/empty"
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { useAction } from "@/hooks/use-action"
import { api, errorText, findMiracle, pickFolder, pickProgram, readShortcut, type PortalApp } from "@/lib/api"

export function AppsView({ data, reload }: ViewProps) {
  const [creating, setCreating] = useState(false)
  const [editing, setEditing] = useState<PortalApp | null>(null)
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null)

  const newAppButton = (
    <Button onClick={() => setCreating(true)}>
      <PlusIcon />
      New app
    </Button>
  )

  return (
    <>
      {data.apps.length === 0 ? (
        <Empty className="border">
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <AppWindowIcon />
            </EmptyMedia>
            <EmptyTitle>No apps yet</EmptyTitle>
            <EmptyDescription>
              Add an app, then tick it for the users who may open it.
            </EmptyDescription>
          </EmptyHeader>
          <EmptyContent>{newAppButton}</EmptyContent>
        </Empty>
      ) : (
        <>
          <div className="flex items-center">
            <p className="text-sm text-muted-foreground">
              An app is an RDP connection to a host, opened as the user’s Windows account.
            </p>
            <div className="ml-auto">{newAppButton}</div>
          </div>
          <div className="overflow-hidden rounded-lg border">
            <Table>
              <TableHeader className="bg-muted/50">
                <TableRow>
                  <TableHead>Name</TableHead>
                  <TableHead>Key</TableHead>
                  <TableHead>RDP host</TableHead>
                  <TableHead>Users</TableHead>
                  <TableHead className="w-20" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {data.apps.map((app) => {
                  const count = data.users.filter((u) => u.app_ids.includes(app.id)).length
                  return (
                    <TableRow key={app.id}>
                      <TableCell className="font-medium">{app.name}</TableCell>
                      <TableCell>
                        <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-xs">{app.key}</code>
                      </TableCell>
                      <TableCell className="text-muted-foreground">
                        <div>
                          {app.hostname}:{app.port}
                        </div>
                        {app.program && (
                          <div className="text-xs">
                            {app.profile === "miracle" ? "Miracle" : "Black screen"}:{" "}
                            <span className="font-mono">
                              {app.profile === "miracle" && app.work_dir ? app.work_dir : app.program}
                            </span>
                          </div>
                        )}
                      </TableCell>
                      <TableCell className="text-muted-foreground tabular-nums">
                        {count === 1 ? "1 user" : `${count} users`}
                      </TableCell>
                      <TableCell className="whitespace-nowrap">
                        <Tooltip>
                          <TooltipTrigger asChild>
                            <Button
                              variant="ghost"
                              size="icon-sm"
                              aria-label={`Edit ${app.name}`}
                              onClick={() => setEditing(app)}
                            >
                              <PencilIcon />
                            </Button>
                          </TooltipTrigger>
                          <TooltipContent>Edit app</TooltipContent>
                        </Tooltip>
                        <Tooltip>
                          <TooltipTrigger asChild>
                            <Button
                              variant="ghost"
                              size="icon-sm"
                              aria-label={`Delete ${app.name}`}
                              onClick={() =>
                                setConfirmation({
                                  title: `Delete ${app.name}?`,
                                  description: count
                                    ? `It will disappear for the ${count === 1 ? "user" : `${count} users`} who can open it now.`
                                    : "No user has it assigned.",
                                  action: "Delete app",
                                  run: async (_, onStep) => {
                                    await api("DELETE", `/api/admin/apps/${app.id}`, undefined, { onStep })
                                    toast.success("App deleted")
                                    reload().catch(() => {})
                                  },
                                })
                              }
                            >
                              <Trash2Icon />
                            </Button>
                          </TooltipTrigger>
                          <TooltipContent>Delete app</TooltipContent>
                        </Tooltip>
                      </TableCell>
                    </TableRow>
                  )
                })}
              </TableBody>
            </Table>
          </div>
        </>
      )}

      <NewAppDialog open={creating} onOpenChange={setCreating} reload={reload} />
      <NewAppDialog
        open={editing !== null}
        editing={editing}
        onOpenChange={(next) => !next && setEditing(null)}
        reload={reload}
      />
      <ConfirmDialog confirmation={confirmation} onClose={() => setConfirmation(null)} />
    </>
  )
}

const blank = { name: "", key: "", hostname: "127.0.0.1", port: "", program: "", work_dir: "", profile: "miracle" }

/** "Tally Prime" -> "tally-prime": a starting point for the link key. */
function keyFrom(name: string) {
  return name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "")
}

function NewAppDialog({
  open,
  onOpenChange,
  reload,
  editing = null,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  reload: () => Promise<void>
  /** The app being changed; none for a new app. */
  editing?: PortalApp | null
}) {
  const [form, setForm] = useState(blank)
  const [keyTyped, setKeyTyped] = useState(false)
  const [error, setError] = useState("")
  const { busy, step, run } = useAction()

  // Opening the dialog for an app fills in what it has now.
  useEffect(() => {
    if (open && editing) {
      setForm({
        name: editing.name,
        key: editing.key,
        hostname: editing.hostname,
        port: editing.port === "3389" ? "" : editing.port,
        program: editing.program ?? "",
        work_dir: editing.work_dir ?? "",
        profile: editing.profile === "miracle" ? "miracle" : "generic",
      })
      setKeyTyped(true)
      setError("")
    }
  }, [open, editing])

  const miracle = form.profile === "miracle"

  // A Miracle app starts out with Miracle's program, when it can be found here.
  useEffect(() => {
    if (!open || !miracle || form.program) return
    let stale = false
    findMiracle()
      .then((path) => {
        if (path && !stale) setForm((f) => (f.program ? f : { ...f, program: path }))
      })
      .catch(() => {})
    return () => {
      stale = true
    }
  }, [open, miracle, form.program])

  const close = (next: boolean) => {
    if (busy) return
    if (!next) {
      setForm(blank)
      setKeyTyped(false)
      setError("")
    }
    onOpenChange(next)
  }

  // Choosing a Miracle shortcut also fills in the data folder it opens.
  const browseProgram = async () => {
    try {
      const path = await pickProgram()
      if (!path) return
      setForm((f) => ({ ...f, program: path }))
      if (path.toLowerCase().endsWith(".lnk")) {
        const link = await readShortcut(path)
        if (link.data_folder) {
          setForm((f) => ({ ...f, work_dir: link.data_folder ?? f.work_dir }))
          toast.success(`${miracle ? "Company" : "Data"} folder filled in from the shortcut`)
        }
      }
    } catch (e) {
      setError(errorText(e))
    }
  }

  const browseFolder = async () => {
    try {
      const title = miracle ? "Choose the Miracle company folder" : "Choose the folder with this user’s files"
      const path = await pickFolder(title, form.work_dir || "C:\\")
      if (path) setForm((f) => ({ ...f, work_dir: path }))
    } catch (e) {
      setError(errorText(e))
    }
  }

  const submit = async (ev: FormEvent) => {
    ev.preventDefault()
    setError("")
    try {
      await run((onStep) =>
        editing
          ? api("PUT", `/api/admin/apps/${editing.id}`, form, { onStep })
          : api("POST", "/api/admin/apps", form, { onStep }),
      )
      toast.success(`${form.name.trim()} ${editing ? "saved" : "created"}`)
      setForm(blank)
      setKeyTyped(false)
      onOpenChange(false)
      reload().catch(() => {})
    } catch (e) {
      setError(errorText(e))
    }
  }

  return (
    <Dialog open={open} onOpenChange={close}>
      {/* A form: closes only with Cancel, X or Esc. In the desktop app a stray
          outside focus event (the tooltip of the button that opened it, a
          native picker) would otherwise close it a moment after opening. */}
      <DialogContent onInteractOutside={(e) => e.preventDefault()}>
        <form onSubmit={submit} autoComplete="off" className="contents">
          <DialogHeader>
            <DialogTitle>{editing ? `Edit ${editing.name}` : "New app"}</DialogTitle>
            <DialogDescription>
              An RDP connection to a host, opened as the user’s Windows account.
            </DialogDescription>
          </DialogHeader>
          <FieldGroup>
            <div className="grid gap-4 sm:grid-cols-2">
              <Field>
                <FieldLabel htmlFor="na-name">Name</FieldLabel>
                <Input
                  id="na-name"
                  value={form.name}
                  placeholder="Accounts"
                  required
                  onChange={(e) =>
                    setForm((f) => ({
                      ...f,
                      name: e.target.value,
                      key: keyTyped ? f.key : keyFrom(e.target.value),
                    }))
                  }
                />
              </Field>
              <Field>
                <FieldLabel htmlFor="na-key">Key</FieldLabel>
                <Input
                  id="na-key"
                  value={form.key}
                  placeholder="accounts"
                  spellCheck={false}
                  required
                  disabled={editing !== null}
                  onChange={(e) => {
                    setKeyTyped(true)
                    setForm((f) => ({ ...f, key: e.target.value }))
                  }}
                />
                <FieldDescription>Used in the app’s link.</FieldDescription>
              </Field>
              <Field>
                <FieldLabel htmlFor="na-host">RDP host</FieldLabel>
                <Input
                  id="na-host"
                  value={form.hostname}
                  spellCheck={false}
                  required
                  onChange={(e) => setForm((f) => ({ ...f, hostname: e.target.value }))}
                />
              </Field>
              <Field>
                <FieldLabel htmlFor="na-port">Port</FieldLabel>
                <Input
                  id="na-port"
                  value={form.port}
                  placeholder="3389"
                  inputMode="numeric"
                  onChange={(e) => setForm((f) => ({ ...f, port: e.target.value }))}
                />
              </Field>
            </div>
            <Field>
              <FieldLabel>App type</FieldLabel>
              <div className="flex gap-2" role="group" aria-label="App type">
                <Button
                  type="button"
                  variant={miracle ? "default" : "outline"}
                  aria-pressed={miracle}
                  onClick={() => setForm((f) => ({ ...f, profile: "miracle" }))}
                >
                  Miracle
                </Button>
                <Button
                  type="button"
                  variant={miracle ? "outline" : "default"}
                  aria-pressed={!miracle}
                  onClick={() => setForm((f) => ({ ...f, profile: "generic" }))}
                >
                  Other program
                </Button>
              </div>
              <FieldDescription>
                {miracle
                  ? "Miracle opens straight on the company folder below, on a black screen, and is closed the safe way (it never loses unsaved work)."
                  : "Any other program: opened on a black screen as given, and closed with a normal close request."}
              </FieldDescription>
            </Field>
            <Field>
              <FieldLabel htmlFor="na-program">{miracle ? "Miracle program" : "Program (optional)"}</FieldLabel>
              <div className="flex gap-2">
                <Input
                  id="na-program"
                  value={form.program}
                  placeholder={miracle ? "miracle.exe, or a shortcut Miracle made" : "Choose a program or a shortcut"}
                  spellCheck={false}
                  onChange={(e) => setForm((f) => ({ ...f, program: e.target.value }))}
                />
                <Button type="button" variant="outline" onClick={browseProgram} disabled={busy}>
                  <FileIcon />
                  Browse…
                </Button>
              </div>
              <FieldDescription>
                {miracle
                  ? "Filled in automatically when Miracle is found on this computer. A shortcut Miracle made also works."
                  : "Leave blank for the Windows desktop. Set it and the user opens just this program on a black screen — no desktop — signing out when it closes."}{" "}
                Needs the user’s Windows account set to restricted.
              </FieldDescription>
            </Field>
            <Field>
              <FieldLabel htmlFor="na-workdir">{miracle ? "Company folder" : "Data folder"}</FieldLabel>
              <div className="flex gap-2">
                <Input
                  id="na-workdir"
                  value={form.work_dir}
                  placeholder={miracle ? "The folder with this user’s company (CMP…) folders" : "Choose the folder with this user’s files"}
                  spellCheck={false}
                  onChange={(e) => setForm((f) => ({ ...f, work_dir: e.target.value }))}
                />
                <Button type="button" variant="outline" onClick={browseFolder} disabled={busy}>
                  <FolderOpenIcon />
                  Browse…
                </Button>
              </div>
              <FieldDescription>
                {miracle ? "Miracle opens this folder. " : ""}Only users given this app can reach it — not
                the program’s own folder, which everyone needs.
              </FieldDescription>
            </Field>
            {error && <FieldError>{error}</FieldError>}
          </FieldGroup>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => close(false)} disabled={busy}>
              Cancel
            </Button>
            <Button type="submit" className="max-w-full min-w-0" disabled={busy}>
              {busy ? <BusyLabel step={step} fallback="Saving…" /> : editing ? "Save" : "Create app"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
