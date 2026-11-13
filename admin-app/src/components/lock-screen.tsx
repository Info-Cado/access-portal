import { useState } from "react"
import { LockKeyholeIcon } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Field, FieldDescription, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { errorText, unlock } from "@/lib/api"

/** The app's own sign-in, shown when it opens and after it locks itself. */
export function LockScreen({ onUnlocked }: { onUnlocked: () => void }) {
  const [username, setUsername] = useState("admin")
  const [password, setPassword] = useState("")
  const [error, setError] = useState("")
  const [busy, setBusy] = useState(false)

  const submit = async (ev: React.FormEvent) => {
    ev.preventDefault()
    setBusy(true)
    setError("")
    try {
      await unlock(username, password)
      setPassword("")
      onUnlocked()
    } catch (e) {
      setError(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <main className="flex min-h-svh items-center justify-center bg-muted/30 p-6">
      <Card className="w-full max-w-sm">
        <CardHeader>
          <div className="mb-2 flex size-10 items-center justify-center rounded-lg bg-primary text-primary-foreground">
            <LockKeyholeIcon className="size-5" />
          </div>
          <CardTitle>Access Portal</CardTitle>
          <CardDescription>Sign in to manage users and sessions on this computer.</CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={submit}>
            <FieldGroup>
              <Field>
                <FieldLabel htmlFor="lock-user">Username</FieldLabel>
                <Input
                  id="lock-user"
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  autoComplete="username"
                  spellCheck={false}
                />
              </Field>
              <Field data-invalid={error ? true : undefined}>
                <FieldLabel htmlFor="lock-pass">Password</FieldLabel>
                <Input
                  id="lock-pass"
                  type="password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  autoComplete="current-password"
                  autoFocus
                  aria-invalid={error ? true : undefined}
                />
                {error ? <FieldError>{error}</FieldError> : null}
                <FieldDescription>
                  The portal keeps running for your users while the app is locked.
                </FieldDescription>
              </Field>
              <Button type="submit" disabled={busy || !password}>
                {busy ? "Signing in…" : "Sign in"}
              </Button>
            </FieldGroup>
          </form>
        </CardContent>
      </Card>
    </main>
  )
}
