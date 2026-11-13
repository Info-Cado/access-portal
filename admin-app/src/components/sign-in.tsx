import { useState, type FormEvent } from "react"
import { MonitorSmartphoneIcon } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Field, FieldError, FieldGroup, FieldLabel } from "@/components/ui/field"
import { Input } from "@/components/ui/input"
import { ApiError, api, errorText, setServer } from "@/lib/api"

function recall(key: string, fallback = "") {
  try {
    return localStorage.getItem(key) ?? fallback
  } catch {
    return fallback
  }
}

function remember(key: string, value: string) {
  try {
    localStorage.setItem(key, value)
  } catch {
    // Remembering the address is a convenience only.
  }
}

export function SignIn({
  notice,
  onSignedIn,
}: {
  notice: string
  onSignedIn: (server: string) => Promise<void>
}) {
  const [address, setAddress] = useState(() => recall("server", "http://127.0.0.1:8088"))
  const [username, setUsername] = useState(() => recall("user"))
  const [password, setPassword] = useState("")
  const [code, setCode] = useState("")
  const [needCode, setNeedCode] = useState(false)
  const [error, setError] = useState(notice)
  const [busy, setBusy] = useState(false)

  const submit = async (ev: FormEvent) => {
    ev.preventDefault()
    setBusy(true)
    setError("")
    try {
      const server = await setServer(address)
      await api("POST", "/api/login", { username, password, code: code || null })
      remember("server", server)
      remember("user", username.trim())
      await onSignedIn(server)
    } catch (e) {
      if (e instanceof ApiError && e.needCode) setNeedCode(true)
      setError(errorText(e))
      setBusy(false)
    }
  }

  return (
    <main className="flex min-h-svh items-center justify-center bg-muted p-6">
      <div className="flex w-full max-w-sm flex-col gap-6">
        <div className="flex items-center justify-center gap-2 font-medium">
          <div className="flex size-7 items-center justify-center rounded-md bg-primary text-primary-foreground">
            <MonitorSmartphoneIcon className="size-4" />
          </div>
          Access Portal Admin
        </div>
        <Card>
          <CardHeader>
            <CardTitle>Sign in</CardTitle>
            <CardDescription>Use a portal administrator account.</CardDescription>
          </CardHeader>
          <CardContent>
            <form onSubmit={submit} autoComplete="off">
              <FieldGroup>
                <Field>
                  <FieldLabel htmlFor="address">Portal address</FieldLabel>
                  <Input
                    id="address"
                    value={address}
                    onChange={(e) => setAddress(e.target.value)}
                    placeholder="http://127.0.0.1:8088"
                    spellCheck={false}
                    required
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor="username">Username</FieldLabel>
                  <Input
                    id="username"
                    value={username}
                    onChange={(e) => setUsername(e.target.value)}
                    spellCheck={false}
                    autoFocus={!username}
                    required
                  />
                </Field>
                <Field>
                  <FieldLabel htmlFor="password">Password</FieldLabel>
                  <Input
                    id="password"
                    type="password"
                    value={password}
                    onChange={(e) => setPassword(e.target.value)}
                    autoFocus={Boolean(username)}
                    required
                  />
                </Field>
                {needCode && (
                  <Field>
                    <FieldLabel htmlFor="code">Two-step verification code</FieldLabel>
                    <Input
                      id="code"
                      value={code}
                      onChange={(e) => setCode(e.target.value)}
                      inputMode="numeric"
                      maxLength={6}
                      placeholder="6 digits"
                      autoFocus
                    />
                  </Field>
                )}
                {error && <FieldError>{error}</FieldError>}
                <Button type="submit" disabled={busy}>
                  {busy ? "Signing in…" : "Sign in"}
                </Button>
              </FieldGroup>
            </form>
          </CardContent>
        </Card>
      </div>
    </main>
  )
}
