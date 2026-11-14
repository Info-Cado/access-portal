// Every Windows account on this computer, shown under the portal's own users:
// edit details, password, on/off and the full desktop; delete; and open the
// owner's own desktop from the portal.

import { useCallback, useEffect, useState } from "react"
import { ChevronRightIcon, MonitorIcon, RefreshCwIcon, Trash2Icon, UserPlusIcon } from "lucide-react"
import { toast } from "sonner"

import { ConfirmDialog, type Confirmation } from "@/components/dialogs"
import { Badge } from "@/components/ui/badge"
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
  Field,
  FieldContent,
  FieldDescription,
  FieldGroup,
  FieldLabel,
  FieldLegend,
  FieldSeparator,
  FieldSet,
} from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { Skeleton } from "@/components/ui/skeleton"
import { Switch } from "@/components/ui/switch"
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table"
import { api, errorText, since, type WindowsAccount, type WindowsAccounts } from "@/lib/api"

const path = (name: string) => `/api/admin/windows-accounts/${encodeURIComponent(name)}`

export function WindowsAccountsSection({
  reload,
  query,
  onAddLogin,
}: {
  /** Reload the portal's own data (users change when owner access does). */
  reload: () => Promise<void>
  query: string
  /** Open the new-user dialog to add a portal login for a Windows account. */
  onAddLogin: (winUser: string, accountName: string) => void
}) {
  const [list, setList] = useState<WindowsAccounts | null>(null)
  const [error, setError] = useState("")
  const [loading, setLoading] = useState(false)
  const [selected, setSelected] = useState<string | null>(null)

  const load = useCallback(async () => {
    setLoading(true)
    try {
      setList(await api<WindowsAccounts>("GET", "/api/admin/windows-accounts"))
      setError("")
    } catch (e) {
      setError(errorText(e))
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  const refreshAll = async () => {
    await Promise.all([load(), reload()])
  }

  const q = query.trim().toLowerCase()
  const shown = (list?.accounts ?? []).filter(
    (a) =>
      a.me || // the owner's account is always shown
      !q ||
      a.name.toLowerCase().includes(q) ||
      a.full_name.toLowerCase().includes(q) ||
      a.portal_users.some((u) => u.username.toLowerCase().includes(q)),
  )
  const account = list?.accounts.find((a) => a.name === selected) ?? null

  return (
    <section className="flex flex-col gap-3">
      <div className="flex items-center gap-2">
        <div>
          <h2 className="text-sm font-medium">Windows accounts on this computer</h2>
          <p className="text-xs text-muted-foreground">
            Every local account, including ones the portal didn’t make. Windows’ built-in accounts are hidden.
          </p>
        </div>
        <Button variant="outline" size="sm" className="ml-auto" disabled={loading} onClick={() => void load()}>
          <RefreshCwIcon className={loading ? "animate-spin" : undefined} />
          Refresh
        </Button>
      </div>

      {error ? (
        <div className="rounded-lg border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive">
          {error}
        </div>
      ) : list === null ? (
        <div className="flex flex-col gap-2">
          <Skeleton className="h-10 w-full" />
          <Skeleton className="h-10 w-full" />
        </div>
      ) : (
        <div className="overflow-hidden rounded-lg border">
          <Table>
            <TableHeader className="bg-muted/50">
              <TableRow>
                <TableHead>Account</TableHead>
                <TableHead>Portal users</TableHead>
                <TableHead>Last sign-in</TableHead>
                <TableHead className="w-10" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {shown.map((a) => (
                <TableRow
                  key={a.name}
                  tabIndex={0}
                  className="cursor-pointer"
                  data-state={a.name === selected ? "selected" : undefined}
                  onClick={() => setSelected(a.name)}
                  onKeyDown={(ev) => ev.key === "Enter" && setSelected(a.name)}
                >
                  <TableCell>
                    <div className="flex flex-wrap items-center gap-2">
                      <span className="font-medium">{a.name}</span>
                      {a.full_name && a.full_name !== a.name && (
                        <span className="text-muted-foreground">{a.full_name}</span>
                      )}
                      {a.me && <Badge>You</Badge>}
                      {a.me && list.owner_user_id !== null && (
                        <Badge className="border-emerald-500/30 bg-emerald-500/15 text-emerald-600 dark:text-emerald-400">
                          On the portal
                        </Badge>
                      )}
                      {a.is_admin && <Badge variant="secondary">Administrator</Badge>}
                      {a.portal && <Badge variant="outline">Portal</Badge>}
                      {a.microsoft && <Badge variant="outline">Microsoft account</Badge>}
                      {!a.enabled && <Badge variant="destructive">Off</Badge>}
                    </div>
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {a.portal_users.length ? a.portal_users.map((u) => u.username).join(", ") : "None"}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {a.last_logon ? `${since(a.last_logon)}${since(a.last_logon) === "just now" ? "" : " ago"}` : "Never"}
                  </TableCell>
                  <TableCell>
                    <ChevronRightIcon className="size-4 text-muted-foreground" />
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      )}

      <AccountSheet
        account={account}
        ownerUserId={list?.owner_user_id ?? null}
        refresh={refreshAll}
        onClose={() => setSelected(null)}
        onAddLogin={onAddLogin}
      />
    </section>
  )
}

function AccountSheet({
  account,
  ownerUserId,
  refresh,
  onClose,
  onAddLogin,
}: {
  account: WindowsAccount | null
  ownerUserId: number | null
  refresh: () => Promise<void>
  onClose: () => void
  onAddLogin: (winUser: string, accountName: string) => void
}) {
  // Keep showing the last account while the dialog closes.
  const [shown, setShown] = useState(account)
  useEffect(() => {
    if (account) setShown(account)
  }, [account])

  return (
    <Dialog open={account !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent className="flex max-h-[calc(100svh-3rem)] flex-col gap-0 p-0 sm:max-w-2xl">
        {shown && (
          <AccountDetails
            key={shown.name}
            account={shown}
            ownerUserId={ownerUserId}
            refresh={refresh}
            onClose={onClose}
            onAddLogin={onAddLogin}
          />
        )}
      </DialogContent>
    </Dialog>
  )
}

function AccountDetails({
  account,
  ownerUserId,
  refresh,
  onClose,
  onAddLogin,
}: {
  account: WindowsAccount
  ownerUserId: number | null
  refresh: () => Promise<void>
  onClose: () => void
  onAddLogin: (winUser: string, accountName: string) => void
}) {
  const base = path(account.name)
  const [busy, setBusy] = useState(false)
  const [fullName, setFullName] = useState(account.full_name)
  const [description, setDescription] = useState(account.description)
  const [password, setPassword] = useState("")
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null)
  const [ownerDialog, setOwnerDialog] = useState(false)

  const change = async (work: () => Promise<unknown>, done: string) => {
    setBusy(true)
    try {
      await work()
      await refresh()
      toast.success(done)
      return true
    } catch (e) {
      toast.error(errorText(e))
      return false
    } finally {
      setBusy(false)
    }
  }

  const deletable = !account.me && !account.is_admin
  const linked = account.portal_users.map((u) => u.username).join(", ")

  return (
    <>
      <DialogHeader className="border-b p-5 text-left">
        <DialogTitle className="flex flex-wrap items-center gap-2">
          {account.name}
          {account.me && <Badge>You</Badge>}
          {account.is_admin && <Badge variant="secondary">Administrator</Badge>}
          {!account.enabled && <Badge variant="destructive">Off</Badge>}
        </DialogTitle>
        <DialogDescription>
          {account.me
            ? "The account Access Portal runs as on this computer. It is always shown here."
            : account.portal_users.length
              ? `Portal users signing in as it: ${linked}.`
              : "No portal user signs in as this account."}
        </DialogDescription>
      </DialogHeader>

      <div className="flex-1 overflow-y-auto p-5">
        <FieldGroup>
          <FieldSet>
            <FieldLegend variant="label">Portal logins</FieldLegend>
            <FieldDescription>
              {account.portal_users.length
                ? "These portal users open a session as this Windows account. Several can share one account."
                : "No portal user opens a session as this account yet. You can add one."}
            </FieldDescription>
            {account.portal_users.length > 0 && (
              <div className="flex flex-wrap gap-1.5">
                {account.portal_users.map((u) => (
                  <Badge key={u.id} variant="outline">
                    {u.username}
                  </Badge>
                ))}
              </div>
            )}
            <Button
              variant="outline"
              size="sm"
              className="self-start"
              onClick={() => {
                onClose()
                onAddLogin(account.name, account.name)
              }}
            >
              <UserPlusIcon />
              Add portal login
            </Button>
          </FieldSet>
          <FieldSeparator />

          {account.me && (
            <>
              <Field orientation="horizontal">
                <FieldContent>
                  <FieldLabel htmlFor="owner-access" className="flex items-center gap-1.5">
                    <MonitorIcon className="size-4" />
                    Open my desktop from the portal
                  </FieldLabel>
                  <FieldDescription>
                    {ownerUserId !== null
                      ? "On. Sign in to the portal with your portal user and your own Windows desktop opens. Opening it there locks this computer’s screen, as Remote Desktop does."
                      : "Off. Turn it on to sign in to the portal and use your own Windows desktop from anywhere."}
                  </FieldDescription>
                </FieldContent>
                <Switch
                  id="owner-access"
                  checked={ownerUserId !== null}
                  disabled={busy}
                  onCheckedChange={(on) =>
                    on
                      ? setOwnerDialog(true)
                      : setConfirmation({
                          title: "Stop opening your desktop from the portal?",
                          description:
                            "The portal user for your account is deleted. Your Windows account and its files stay as they are.",
                          action: "Turn off",
                          run: async () => {
                            await api("DELETE", "/api/admin/owner-access")
                            await refresh()
                            toast.success("Your desktop is no longer on the portal")
                          },
                        })
                  }
                />
              </Field>
              <FieldSeparator />
            </>
          )}

          {!account.me && (
            <>
              <Field orientation="horizontal">
                <FieldContent>
                  <FieldLabel htmlFor="acct-enabled">Account on</FieldLabel>
                  <FieldDescription>
                    {account.enabled
                      ? "On. It can sign in to this computer and over Remote Desktop."
                      : "Off. Nobody can sign in with it, from the portal or anywhere else."}
                  </FieldDescription>
                </FieldContent>
                <Switch
                  id="acct-enabled"
                  checked={account.enabled}
                  disabled={busy}
                  onCheckedChange={(enabled) =>
                    change(() => api("PUT", base, { enabled }), enabled ? "Account turned on" : "Account turned off")
                  }
                />
              </Field>
              {!account.is_admin && (
                <>
                  <FieldSeparator />
                  <AccountDesktopMode account={account} />
                </>
              )}
              <FieldSeparator />
            </>
          )}

          <form
            autoComplete="off"
            onSubmit={async (ev) => {
              ev.preventDefault()
              await change(() => api("PUT", base, { full_name: fullName, description }), "Details saved")
            }}
          >
            <FieldSet>
              <FieldLegend variant="label">Details</FieldLegend>
              <FieldGroup className="gap-3">
                <Field>
                  <FieldLabel htmlFor="acct-full-name">Full name</FieldLabel>
                  <Input id="acct-full-name" value={fullName} onChange={(e) => setFullName(e.target.value)} />
                </Field>
                <Field>
                  <FieldLabel htmlFor="acct-description">Description</FieldLabel>
                  <Input
                    id="acct-description"
                    value={description}
                    maxLength={48}
                    onChange={(e) => setDescription(e.target.value)}
                  />
                  <FieldDescription>Up to 48 characters.</FieldDescription>
                </Field>
                <Button
                  type="submit"
                  variant="outline"
                  className="self-start"
                  disabled={busy || (fullName === account.full_name && description === account.description)}
                >
                  Save details
                </Button>
              </FieldGroup>
            </FieldSet>
          </form>

          <FieldSeparator />

          {account.microsoft ? (
            <Field>
              <FieldLabel>Windows password</FieldLabel>
              <FieldDescription>
                This account signs in with a Microsoft account; its password is changed at account.microsoft.com.
              </FieldDescription>
            </Field>
          ) : (
            <form
              autoComplete="off"
              onSubmit={async (ev) => {
                ev.preventDefault()
                if (await change(() => api("PUT", base, { password }), "Windows password changed")) setPassword("")
              }}
            >
              <Field>
                <FieldLabel htmlFor="acct-password">Windows password</FieldLabel>
                <div className="flex gap-2">
                  <Input
                    id="acct-password"
                    type="password"
                    placeholder="New password"
                    autoComplete="new-password"
                    value={password}
                    onChange={(e) => setPassword(e.target.value)}
                  />
                  <Button type="submit" variant="outline" disabled={busy || !password}>
                    Set password
                  </Button>
                </div>
                <FieldDescription>
                  {account.me
                    ? "The password you sign in to this computer with. The portal keeps using the new one for your desktop."
                    : "Portal users signing in as this account get the new password too, so their sessions keep working."}
                </FieldDescription>
              </Field>
            </form>
          )}
        </FieldGroup>
      </div>

      <DialogFooter className="mx-0 mb-0 border-t sm:justify-start">
        <Button
          variant="destructive"
          disabled={busy || !deletable}
          title={
            account.me
              ? "This is the account Access Portal runs as."
              : account.is_admin
                ? "Windows administrator accounts can't be deleted from here."
                : undefined
          }
          onClick={() =>
            setConfirmation({
              title: `Delete Windows account ${account.name}?`,
              description:
                "It is signed out first if it is signed in (unsaved work is lost), then deleted from this computer." +
                (account.portal_users.length
                  ? ` Portal users signing in as it (${linked}) stay, but can't connect until they get another account.`
                  : ""),
              action: "Delete account",
              option: { label: "Also delete its files (its user folder)", checked: false },
              run: async (files) => {
                await api("DELETE", files ? `${base}?files=1` : base)
                onClose()
                await refresh()
                toast.success(`Windows account ${account.name} deleted`)
              },
            })
          }
        >
          <Trash2Icon />
          Delete account
        </Button>
        {!deletable && (
          <p className="text-xs text-muted-foreground">
            {account.me
              ? "This is the account Access Portal runs as, so it can't be deleted."
              : "Windows administrator accounts can't be deleted from here."}
          </p>
        )}
      </DialogFooter>

      <OwnerAccessDialog
        account={account}
        open={ownerDialog}
        onOpenChange={setOwnerDialog}
        done={refresh}
      />
      <ConfirmDialog confirmation={confirmation} onClose={() => setConfirmation(null)} />
    </>
  )
}

type Mode = "app" | "full" | "missing"

/** The same Full desktop switch the portal users' panel has, by account name. */
function AccountDesktopMode({ account }: { account: WindowsAccount }) {
  const [mode, setMode] = useState<Mode | null>(null)
  const [error, setError] = useState("")
  const [busy, setBusy] = useState(false)
  const url = `${path(account.name)}/desktop`

  useEffect(() => {
    let live = true
    api<{ mode: Mode }>("GET", url)
      .then((r) => live && setMode(r.mode))
      .catch((e) => live && setError(errorText(e)))
    return () => {
      live = false
    }
  }, [url])

  const change = async (full: boolean) => {
    setBusy(true)
    try {
      setMode((await api<{ mode: Mode }>("PUT", url, { full })).mode)
      toast.success(full ? "Full desktop turned on" : "Full desktop turned off", {
        description: "Applies from its next session.",
      })
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Field orientation="horizontal">
      <FieldContent>
        <FieldLabel htmlFor="acct-full-desktop">Full desktop</FieldLabel>
        <FieldDescription className={error ? "text-destructive" : undefined}>
          {error ||
            (mode === null
              ? "Checking…"
              : mode === "full"
                ? "On. Portal users signing in as it get Desktop, the normal Windows desktop."
                : "Off. Run, command prompt, Task Manager and drive C: are blocked, and the portal offers only assigned apps.")}
        </FieldDescription>
      </FieldContent>
      {mode === null && !error ? (
        <Skeleton className="h-[1.15rem] w-8 rounded-full" />
      ) : (
        <Switch
          id="acct-full-desktop"
          checked={mode === "full"}
          disabled={busy || (mode !== "app" && mode !== "full")}
          onCheckedChange={change}
        />
      )}
    </Field>
  )
}

/** Turn on opening the owner's own desktop from the portal. */
function OwnerAccessDialog({
  account,
  open,
  onOpenChange,
  done,
}: {
  account: WindowsAccount
  open: boolean
  onOpenChange: (open: boolean) => void
  done: () => Promise<void>
}) {
  const blank = { username: account.name.toLowerCase(), password: "", winPassword: "", winUsername: "" }
  const [form, setForm] = useState(blank)
  const [busy, setBusy] = useState(false)
  useEffect(() => {
    if (open) setForm(blank)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])
  const set = (patch: Partial<typeof blank>) => setForm((f) => ({ ...f, ...patch }))

  const submit = async () => {
    setBusy(true)
    try {
      await api("POST", "/api/admin/owner-access", {
        username: form.username,
        password: form.password,
        win_password: form.winPassword,
        win_username: form.winUsername,
      })
      await done()
      onOpenChange(false)
      toast.success("Your desktop is on the portal", {
        description: `Sign in to the portal as ${form.username.trim()} to open it.`,
      })
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={open} onOpenChange={(o) => !busy && onOpenChange(o)}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Open your desktop from the portal</DialogTitle>
          <DialogDescription>
            Makes a portal user that opens your own Windows account, {account.name}, with its full desktop.
          </DialogDescription>
        </DialogHeader>
        <form
          autoComplete="off"
          onSubmit={(ev) => {
            ev.preventDefault()
            void submit()
          }}
        >
          <FieldGroup className="gap-4">
            <div className="grid gap-4 sm:grid-cols-2">
              <Field>
                <FieldLabel htmlFor="oa-user">Portal username</FieldLabel>
                <Input
                  id="oa-user"
                  value={form.username}
                  spellCheck={false}
                  onChange={(e) => set({ username: e.target.value })}
                />
              </Field>
              <Field>
                <FieldLabel htmlFor="oa-pass">Portal password</FieldLabel>
                <Input
                  id="oa-pass"
                  type="password"
                  autoComplete="new-password"
                  value={form.password}
                  onChange={(e) => set({ password: e.target.value })}
                />
              </Field>
            </div>
            <Field>
              <FieldLabel htmlFor="oa-win-pass">Your Windows password</FieldLabel>
              <Input
                id="oa-win-pass"
                type="password"
                autoComplete="new-password"
                value={form.winPassword}
                onChange={(e) => set({ winPassword: e.target.value })}
              />
              <FieldDescription>
                {account.microsoft
                  ? "Your Microsoft account password (not your PIN). It is stored encrypted and never shown again."
                  : "The password you sign in to this computer with (not your PIN). It is checked now, stored encrypted and never shown again."}
              </FieldDescription>
            </Field>
            {account.microsoft && (
              <Field>
                <FieldLabel htmlFor="oa-win-user">Microsoft account email</FieldLabel>
                <Input
                  id="oa-win-user"
                  value={form.winUsername}
                  spellCheck={false}
                  placeholder="you@example.com"
                  onChange={(e) => set({ winUsername: e.target.value })}
                />
                <FieldDescription>Remote Desktop signs in to a Microsoft account with its email address.</FieldDescription>
              </Field>
            )}
          </FieldGroup>
          <DialogFooter className="mt-6">
            <Button type="button" variant="outline" disabled={busy} onClick={() => onOpenChange(false)}>
              Cancel
            </Button>
            <Button
              type="submit"
              disabled={busy || !form.username.trim() || !form.password || !form.winPassword}
            >
              Turn on
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
