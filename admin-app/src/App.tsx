import { useCallback, useEffect, useState } from "react"

import { AppShell } from "@/components/app-shell"
import { SignIn } from "@/components/sign-in"
import { Toaster } from "@/components/ui/sonner"
import { TooltipProvider } from "@/components/ui/tooltip"
import { api, setSignedOutHandler, type PortalState } from "@/lib/api"

export default function App() {
  const [server, setServer] = useState("")
  const [data, setData] = useState<PortalState | null>(null)
  const [notice, setNotice] = useState("")

  const reload = useCallback(async () => {
    setData(await api<PortalState>("GET", "/api/admin/state"))
  }, [])

  useEffect(() => {
    setSignedOutHandler(() => {
      setData(null)
      setNotice("Your sign-in expired. Sign in again.")
    })
  }, [])

  const signOut = async () => {
    await api("POST", "/api/logout").catch(() => {})
    setNotice("")
    setData(null)
  }

  return (
    <TooltipProvider>
      {data ? (
        <AppShell data={data} server={server} reload={reload} onSignOut={signOut} />
      ) : (
        <SignIn
          notice={notice}
          onSignedIn={async (address) => {
            setServer(address)
            setNotice("")
            await reload()
          }}
        />
      )}
      <Toaster position="bottom-center" />
    </TooltipProvider>
  )
}
