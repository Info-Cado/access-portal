import { useState } from "react"
import { ChevronRightIcon, MessageSquareTextIcon, PlusIcon, SearchIcon, UserXIcon } from "lucide-react"

import type { ViewProps } from "@/components/app-shell"
import { SecretDialog, type SecretToShow } from "@/components/dialogs"
import { MessageDialog, type MessageTarget } from "@/components/message-dialog"
import { NewUserDialog } from "@/components/new-user-dialog"
import { UserSheet } from "@/components/user-sheet"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty"
import { InputGroup, InputGroupAddon, InputGroupInput } from "@/components/ui/input-group"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"

export function UsersView({ data, reload }: ViewProps) {
  const [query, setQuery] = useState("")
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [creating, setCreating] = useState(false)
  const [secret, setSecret] = useState<SecretToShow | null>(null)
  const [messageTo, setMessageTo] = useState<MessageTarget | null>(null)
  const connected = data.users.reduce((n, u) => n + u.sessions.length, 0)

  const q = query.trim().toLowerCase()
  const shown = data.users.filter(
    (u) => !q || u.username.toLowerCase().includes(q) || u.win_username.toLowerCase().includes(q),
  )
  const selected = data.users.find((u) => u.id === selectedId) ?? null

  return (
    <>
      <div className="flex items-center gap-2">
        <InputGroup className="max-w-xs">
          <InputGroupAddon>
            <SearchIcon />
          </InputGroupAddon>
          <InputGroupInput
            placeholder="Search users"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            spellCheck={false}
          />
        </InputGroup>
        <Button
          variant="outline"
          className="ml-auto"
          disabled={connected === 0}
          title={connected === 0 ? "Nobody is connected right now" : undefined}
          onClick={() => setMessageTo({ label: `everyone connected (${connected})` })}
        >
          <MessageSquareTextIcon />
          Message everyone
        </Button>
        <Button onClick={() => setCreating(true)}>
          <PlusIcon />
          New user
        </Button>
      </div>

      {shown.length === 0 ? (
        <Empty className="border">
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <UserXIcon />
            </EmptyMedia>
            <EmptyTitle>{q ? "No matching users" : "No users yet"}</EmptyTitle>
            <EmptyDescription>
              {q ? `Nothing matches “${query.trim()}”.` : "Create the first user to get started."}
            </EmptyDescription>
          </EmptyHeader>
        </Empty>
      ) : (
        <div className="overflow-hidden rounded-lg border">
          <Table>
            <TableHeader className="bg-muted/50">
              <TableRow>
                <TableHead>User</TableHead>
                <TableHead>Windows login</TableHead>
                <TableHead>Apps</TableHead>
                <TableHead className="w-10" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {shown.map((u) => {
                const apps = data.apps.filter((a) => u.app_ids.includes(a.id))
                return (
                  <TableRow
                    key={u.id}
                    tabIndex={0}
                    className="cursor-pointer"
                    data-state={u.id === selectedId ? "selected" : undefined}
                    onClick={() => setSelectedId(u.id)}
                    onKeyDown={(ev) => ev.key === "Enter" && setSelectedId(u.id)}
                  >
                    <TableCell>
                      <div className="flex items-center gap-2">
                        {u.sessions.length > 0 && (
                          <span className="relative flex size-2" title="Connected now">
                            <span className="absolute inline-flex size-full animate-ping rounded-full bg-emerald-500 opacity-60" />
                            <span className="relative inline-flex size-2 rounded-full bg-emerald-500" />
                          </span>
                        )}
                        <span className="font-medium">{u.username}</span>
                        {u.sessions.length > 0 && (
                          <Badge className="border-emerald-500/30 bg-emerald-500/15 text-emerald-600 dark:text-emerald-400">
                            Connected{u.sessions.length > 1 ? ` ×${u.sessions.length}` : ""}
                          </Badge>
                        )}
                        {u.totp && <Badge variant="secondary">2-step</Badge>}
                        {u.locked && <Badge variant="destructive">Locked</Badge>}
                      </div>
                    </TableCell>
                    <TableCell className="text-muted-foreground">{u.win_username}</TableCell>
                    <TableCell>
                      {apps.length ? (
                        <div className="flex flex-wrap gap-1">
                          {apps.map((a) => (
                            <Badge key={a.id} variant="outline">
                              {a.name}
                            </Badge>
                          ))}
                        </div>
                      ) : (
                        <span className="text-muted-foreground">None</span>
                      )}
                    </TableCell>
                    <TableCell>
                      <ChevronRightIcon className="size-4 text-muted-foreground" />
                    </TableCell>
                  </TableRow>
                )
              })}
            </TableBody>
          </Table>
        </div>
      )}

      <UserSheet
        user={selected}
        data={data}
        reload={reload}
        onClose={() => setSelectedId(null)}
        onSecret={setSecret}
      />
      <NewUserDialog
        open={creating}
        onOpenChange={setCreating}
        apps={data.apps}
        reload={reload}
        onSecret={setSecret}
      />
      <SecretDialog secret={secret} onClose={() => setSecret(null)} />
      <MessageDialog target={messageTo} onClose={() => setMessageTo(null)} />
    </>
  )
}
