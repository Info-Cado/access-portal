import { useCallback, useEffect, useState } from "react"
import { DownloadIcon, HistoryIcon, SearchIcon } from "lucide-react"
import { toast } from "sonner"

import type { ViewProps } from "@/components/app-shell"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty"
import { InputGroup, InputGroupAddon, InputGroupInput } from "@/components/ui/input-group"
import { NativeSelect } from "@/components/ui/native-select"
import { Skeleton } from "@/components/ui/skeleton"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { api, errorText, saveDownload, type ActivityEntry } from "@/lib/api"

const PAGE = 200

/** What each kind of entry is called, and how it is shown. */
const KINDS: Record<string, { label: string; tone?: "bad" | "warn" | "admin" }> = {
  signin: { label: "Signed in" },
  signin_failed: { label: "Sign-in failed", tone: "bad" },
  signin_denied: { label: "Not allowed now", tone: "warn" },
  signout: { label: "Signed out" },
  session_start: { label: "Session opened" },
  session_end: { label: "Session closed" },
  print: { label: "Printed" },
  upload: { label: "Uploaded" },
  download: { label: "Downloaded" },
  clipboard: { label: "Clipboard" },
  auto_signout: { label: "Auto sign-out", tone: "warn" },
  admin: { label: "Admin", tone: "admin" },
}

function kindBadge(kind: string) {
  const k = KINDS[kind] ?? { label: kind }
  const cls =
    k.tone === "bad"
      ? "border-destructive/30 bg-destructive/10 text-destructive"
      : k.tone === "warn"
        ? "border-amber-500/30 bg-amber-500/10 text-amber-700 dark:text-amber-400"
        : k.tone === "admin"
          ? "border-primary/30 bg-primary/10 text-primary"
          : undefined
  return (
    <Badge variant="outline" className={cls}>
      {k.label}
    </Badge>
  )
}

function when(at: number) {
  const d = new Date(at * 1000)
  const today = new Date()
  const sameDay = d.toDateString() === today.toDateString()
  return sameDay
    ? d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
    : d.toLocaleString([], { day: "numeric", month: "short", hour: "2-digit", minute: "2-digit" })
}

function csvCell(v: string | number | null) {
  const s = v === null ? "" : String(v)
  return /[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s
}

export function ActivityView({ data }: ViewProps) {
  const [entries, setEntries] = useState<ActivityEntry[] | null>(null)
  const [more, setMore] = useState(false)
  const [userId, setUserId] = useState<string>("")
  const [query, setQuery] = useState("")
  const [busy, setBusy] = useState(false)

  const load = useCallback(
    async (before?: number) => {
      const params = new URLSearchParams({ limit: String(PAGE) })
      if (userId) params.set("user", userId)
      if (before) params.set("before", String(before))
      const r = await api<{ entries: ActivityEntry[] }>("GET", `/api/admin/activity?${params}`)
      setMore(r.entries.length === PAGE)
      return r.entries
    },
    [userId],
  )

  useEffect(() => {
    let live = true
    setEntries(null)
    load()
      .then((e) => live && setEntries(e))
      .catch((e) => toast.error(errorText(e)))
    // New entries appear while the screen is open.
    const timer = setInterval(() => {
      load()
        .then((fresh) =>
          setEntries((old) => {
            if (!live || !old) return old
            const newest = old[0]?.id ?? 0
            return [...fresh.filter((e) => e.id > newest), ...old]
          }),
        )
        .catch(() => {})
    }, 10_000)
    return () => {
      live = false
      clearInterval(timer)
    }
  }, [load])

  const q = query.trim().toLowerCase()
  const shown = (entries ?? []).filter(
    (e) =>
      !q ||
      e.username.toLowerCase().includes(q) ||
      e.detail.toLowerCase().includes(q) ||
      e.address.toLowerCase().includes(q) ||
      (KINDS[e.kind]?.label ?? e.kind).toLowerCase().includes(q),
  )

  const loadMore = async () => {
    if (!entries?.length) return
    setBusy(true)
    try {
      const older = await load(entries[entries.length - 1].id)
      setEntries([...entries, ...older])
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const exportCsv = async () => {
    setBusy(true)
    try {
      // Everything matching the user filter, not just what is loaded.
      const params = new URLSearchParams({ limit: "5000" })
      if (userId) params.set("user", userId)
      const all = (await api<{ entries: ActivityEntry[] }>("GET", `/api/admin/activity?${params}`)).entries
      const rows = [
        ["Time", "User", "Event", "Details", "From"],
        ...all.map((e) => [
          new Date(e.at * 1000).toLocaleString(),
          e.username,
          KINDS[e.kind]?.label ?? e.kind,
          e.detail,
          e.address,
        ]),
      ]
      const csv = "﻿" + rows.map((r) => r.map(csvCell).join(",")).join("\r\n") + "\r\n"
      const stamp = new Date().toISOString().slice(0, 10)
      const path = await saveDownload(`Access Portal activity ${stamp}.csv`, csv)
      toast.success("Saved to Downloads", { description: path })
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <div className="flex flex-wrap items-center gap-2">
        <NativeSelect className="w-48" value={userId} onChange={(e) => setUserId(e.target.value)}>
          <option value="">Everyone</option>
          {data.users.map((u) => (
            <option key={u.id} value={u.id}>
              {u.username}
            </option>
          ))}
        </NativeSelect>
        <InputGroup className="max-w-xs">
          <InputGroupAddon>
            <SearchIcon />
          </InputGroupAddon>
          <InputGroupInput
            placeholder="Search activity"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            spellCheck={false}
          />
        </InputGroup>
        <Button variant="outline" className="ml-auto" onClick={exportCsv} disabled={busy}>
          <DownloadIcon />
          Export CSV
        </Button>
      </div>

      {entries === null ? (
        <Skeleton className="h-64 w-full" />
      ) : shown.length === 0 ? (
        <Empty className="border">
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <HistoryIcon />
            </EmptyMedia>
            <EmptyTitle>{q ? "Nothing matches" : "No activity yet"}</EmptyTitle>
            <EmptyDescription>
              Sign-ins, sessions, print jobs, file transfers and admin actions show up here.
            </EmptyDescription>
          </EmptyHeader>
        </Empty>
      ) : (
        <div className="overflow-hidden rounded-lg border">
          <Table>
            <TableHeader className="bg-muted/50">
              <TableRow>
                <TableHead className="w-28">Time</TableHead>
                <TableHead className="w-32">User</TableHead>
                <TableHead className="w-36">Event</TableHead>
                <TableHead>Details</TableHead>
                <TableHead className="w-36">From</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {shown.map((e) => (
                <TableRow key={e.id}>
                  <TableCell className="text-muted-foreground tabular-nums" title={new Date(e.at * 1000).toLocaleString()}>
                    {when(e.at)}
                  </TableCell>
                  <TableCell className="font-medium">{e.username || "—"}</TableCell>
                  <TableCell>{kindBadge(e.kind)}</TableCell>
                  <TableCell className="max-w-md whitespace-normal">{e.detail}</TableCell>
                  <TableCell className="text-muted-foreground">{e.address === "local" ? "this computer" : e.address}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}
      {more && !q && (
        <Button variant="outline" className="self-center" onClick={loadMore} disabled={busy}>
          Show older
        </Button>
      )}
    </>
  )
}
