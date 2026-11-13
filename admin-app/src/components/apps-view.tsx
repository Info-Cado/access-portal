import { useState, type FormEvent } from "react"
import { AppWindowIcon, PlusIcon, Trash2Icon } from "lucide-react"
import { toast } from "sonner"

import type { ViewProps } from "@/components/app-shell"
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
import { api, errorText } from "@/lib/api"

export function AppsView({ data, reload }: ViewProps) {
  const [creating, setCreating] = useState(false)
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
                  <TableHead className="w-10" />
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
                        {app.hostname}:{app.port}
                      </TableCell>
                      <TableCell className="text-muted-foreground tabular-nums">
                        {count === 1 ? "1 user" : `${count} users`}
                      </TableCell>
                      <TableCell>
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
                                  run: async () => {
                                    await api("DELETE", `/api/admin/apps/${app.id}`)
                                    await reload()
                                    toast.success("App deleted")
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
      <ConfirmDialog confirmation={confirmation} onClose={() => setConfirmation(null)} />
    </>
  )
}

const blank = { name: "", key: "", hostname: "127.0.0.1", port: "" }

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
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  reload: () => Promise<void>
}) {
  const [form, setForm] = useState(blank)
  const [keyTyped, setKeyTyped] = useState(false)
  const [error, setError] = useState("")
  const [busy, setBusy] = useState(false)

  const close = (next: boolean) => {
    if (busy) return
    if (!next) {
      setForm(blank)
      setKeyTyped(false)
      setError("")
    }
    onOpenChange(next)
  }

  const submit = async (ev: FormEvent) => {
    ev.preventDefault()
    setBusy(true)
    setError("")
    try {
      await api("POST", "/api/admin/apps", form)
      await reload()
      toast.success(`${form.name.trim()} created`)
      setForm(blank)
      setKeyTyped(false)
      onOpenChange(false)
    } catch (e) {
      setError(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={close}>
      <DialogContent>
        <form onSubmit={submit} autoComplete="off" className="contents">
          <DialogHeader>
            <DialogTitle>New app</DialogTitle>
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
                  placeholder="Miracle"
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
                  placeholder="miracle"
                  spellCheck={false}
                  required
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
            {error && <FieldError>{error}</FieldError>}
          </FieldGroup>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={() => close(false)} disabled={busy}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy}>
              {busy ? "Creating…" : "Create app"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
