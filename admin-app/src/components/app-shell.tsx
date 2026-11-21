import { useCallback, useEffect, useState } from "react"
import { AppWindowIcon, HistoryIcon, MonitorSmartphoneIcon, SettingsIcon, ShieldIcon, TriangleAlertIcon, UsersIcon } from "lucide-react"
import { toast } from "sonner"

import { ActivityView } from "@/components/activity-view"
import { AppsView } from "@/components/apps-view"
import { SettingsDialog, type SettingsSection } from "@/components/settings-dialog"
import type { SetupCheck } from "@/components/setup-section"
import { UsersView } from "@/components/users-view"
import { Button } from "@/components/ui/button"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarRail,
} from "@/components/ui/sidebar"
import { PublicLinkFooter } from "@/components/public-link"
import { api, errorText, restartAsAdmin, type PortalState, type PublicLink } from "@/lib/api"

type View = "users" | "apps" | "activity"

export type ViewProps = { data: PortalState; reload: () => Promise<void> }

/** The "This computer" checks, shared by the problems banner and Settings. */
export type SetupState = {
  checks: SetupCheck[] | null
  checking: boolean
  error: string
  /** Check again; answers the checks. */
  check: () => Promise<void>
  /** Take over checks a fix answered. */
  setChecks: (checks: SetupCheck[]) => void
}

const TITLES: Record<View, string> = { users: "Users", apps: "Apps", activity: "Activity" }

export function AppShell({
  data,
  status,
  reload,
}: ViewProps & { status: { address: string; public: PublicLink; elevated: boolean } }) {
  const [view, setView] = useState<View>("users")
  const [settingsOpen, setSettingsOpen] = useState(false)
  const [settingsSection, setSettingsSection] = useState<SettingsSection>("computer")
  const openSettings = (section?: SettingsSection) => {
    if (section) setSettingsSection(section)
    setSettingsOpen(true)
  }
  // Is this computer fully set up? Checked once when the app opens; Settings
  // shows the same result.
  const [checks, setChecks] = useState<SetupCheck[] | null>(null)
  const [checking, setChecking] = useState(true)
  const [setupError, setSetupError] = useState("")
  const checkSetup = useCallback(async () => {
    setChecking(true)
    try {
      setChecks((await api<{ checks: SetupCheck[] }>("GET", "/api/admin/setup")).checks)
      setSetupError("")
    } catch (e) {
      setSetupError(errorText(e))
    } finally {
      setChecking(false)
    }
  }, [])
  useEffect(() => {
    void checkSetup()
  }, [checkSetup])
  const setup: SetupState = { checks, checking, error: setupError, check: checkSetup, setChecks }
  const setupProblems = checks?.filter((c) => !c.ok && c.fix).length ?? 0

  const [elevating, setElevating] = useState(false)
  const elevate = async () => {
    setElevating(true)
    try {
      await restartAsAdmin()
    } catch (e) {
      toast.error(errorText(e))
      setElevating(false)
    }
  }

  const nav: { id: View; label: string; icon: typeof UsersIcon }[] = [
    { id: "users", label: "Users", icon: UsersIcon },
    { id: "apps", label: "Apps", icon: AppWindowIcon },
    { id: "activity", label: "Activity", icon: HistoryIcon },
  ]

  return (
    <SidebarProvider>
      <Sidebar collapsible="icon">
        <SidebarHeader>
          <SidebarMenu>
            <SidebarMenuItem>
              <SidebarMenuButton size="lg" className="pointer-events-none">
                <div className="flex aspect-square size-8 items-center justify-center rounded-lg bg-sidebar-primary text-sidebar-primary-foreground">
                  <MonitorSmartphoneIcon className="size-4" />
                </div>
                <span className="truncate text-sm font-medium">Access Portal</span>
              </SidebarMenuButton>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarHeader>

        <SidebarContent>
          <SidebarGroup>
            <SidebarGroupContent>
              <SidebarMenu>
                {nav.map((item) => (
                  <SidebarMenuItem key={item.id}>
                    <SidebarMenuButton
                      isActive={view === item.id}
                      tooltip={item.label}
                      onClick={() => setView(item.id)}
                    >
                      <item.icon />
                      <span>{item.label}</span>
                    </SidebarMenuButton>
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>

        <SidebarFooter>
          <PublicLinkFooter link={status.public} local={status.address} direct={data.network.urls} />
          <SidebarMenu>
            <SidebarMenuItem>
              <SidebarMenuButton tooltip="Settings" isActive={settingsOpen} onClick={() => openSettings()}>
                <SettingsIcon />
                <span>Settings</span>
                {setupProblems > 0 && <span className="ml-auto size-1.5 rounded-full bg-amber-500" />}
              </SidebarMenuButton>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarFooter>
        <SidebarRail />
      </Sidebar>

      <SidebarInset className="min-w-0">
        <header className="flex h-14 shrink-0 items-center border-b px-6">
          <h1 className="text-base font-medium">{TITLES[view]}</h1>
        </header>
        <div className="flex flex-1 flex-col gap-4 p-6">
          {!status.elevated && (
            <div className="flex items-center gap-3 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm">
              <ShieldIcon className="size-4 shrink-0 text-amber-600 dark:text-amber-400" />
              <span className="flex-1">
                Access Portal isn’t running as administrator, so it can’t change Windows accounts or Remote Desktop
                settings.
              </span>
              <Button size="sm" disabled={elevating} onClick={elevate}>
                {elevating ? "Restarting…" : "Restart as administrator"}
              </Button>
            </div>
          )}
          {setupProblems > 0 && (
            <div className="flex items-center gap-3 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm">
              <TriangleAlertIcon className="size-4 shrink-0 text-amber-600 dark:text-amber-400" />
              <span className="flex-1">
                This computer isn’t fully set up for the portal ({setupProblems} to fix)
              </span>
              <Button size="sm" variant="outline" onClick={() => openSettings("computer")}>
                Set it up
              </Button>
            </div>
          )}
          {view === "users" && <UsersView data={data} reload={reload} />}
          {view === "apps" && <AppsView data={data} reload={reload} />}
          {view === "activity" && <ActivityView data={data} reload={reload} />}
        </div>
        <SettingsDialog
          open={settingsOpen}
          onOpenChange={setSettingsOpen}
          section={settingsSection}
          onSection={setSettingsSection}
          data={data}
          reload={reload}
          setup={setup}
        />
      </SidebarInset>
    </SidebarProvider>
  )
}
