import { useCallback, useEffect, useState } from "react"
import { ServerCrashIcon } from "lucide-react"

import { AppShell } from "@/components/app-shell"
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from "@/components/ui/empty"
import { Skeleton } from "@/components/ui/skeleton"
import { Toaster } from "@/components/ui/sonner"
import { TooltipProvider } from "@/components/ui/tooltip"
import { api, errorText, serverStatus, type PortalState, type ServerStatus } from "@/lib/api"

export default function App() {
  const [status, setStatus] = useState<ServerStatus | null>(null)
  const [data, setData] = useState<PortalState | null>(null)
  const [error, setError] = useState("")

  const reload = useCallback(async () => {
    setData(await api<PortalState>("GET", "/api/admin/state"))
  }, [])

  const refresh = useCallback(async () => {
    const s = await serverStatus()
    setStatus(s)
    if (s.running) await reload()
    return s
  }, [reload])

  useEffect(() => {
    refresh().catch((e) => setError(errorText(e)))
    // Keep the public link's state and who is connected fresh.
    const timer = setInterval(() => {
      if (document.visibilityState === "visible") refresh().catch(() => {})
    }, 5000)
    // Refresh the moment the window is shown again from the tray.
    const onVisible = () => document.visibilityState === "visible" && refresh().catch(() => {})
    document.addEventListener("visibilitychange", onVisible)
    return () => {
      clearInterval(timer)
      document.removeEventListener("visibilitychange", onVisible)
    }
  }, [refresh])

  let body
  if (error || (status && !status.running)) {
    body = (
      <main className="flex min-h-svh items-center justify-center p-6">
        <Empty>
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <ServerCrashIcon />
            </EmptyMedia>
            <EmptyTitle>The portal isn’t running</EmptyTitle>
            <EmptyDescription>
              {error || (status && !status.running ? status.error : "")}
            </EmptyDescription>
            {status && (
              <EmptyDescription>
                Settings and the log are in <code>{status.data_dir}</code>. Fix the problem, then quit
                Access Portal from its tray icon and open it again.
              </EmptyDescription>
            )}
          </EmptyHeader>
        </Empty>
      </main>
    )
  } else if (data && status?.running) {
    body = (
      <AppShell data={data} status={status} reload={reload} />
    )
  } else {
    body = (
      <main className="flex min-h-svh flex-col gap-3 p-6">
        <Skeleton className="h-8 w-48" />
        <Skeleton className="h-64 w-full" />
      </main>
    )
  }

  return (
    <TooltipProvider>
      {body}
      <Toaster position="bottom-center" />
    </TooltipProvider>
  )
}
