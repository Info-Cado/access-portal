import { useEffect, useState } from "react"
import {
  AppWindowIcon,
  HistoryIcon,
  LockIcon,
  MonitorSmartphoneIcon,
  RefreshCwIcon,
  SettingsIcon,
  TriangleAlertIcon,
  UsersIcon,
} from "lucide-react"
import { toast } from "sonner"

import { ActivityView } from "@/components/activity-view"
import { AppsView } from "@/components/apps-view"
import { SettingsView } from "@/components/settings-view"
import { UsersView } from "@/components/users-view"
import { Button } from "@/components/ui/button"
import { Separator } from "@/components/ui/separator"
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupContent,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarInset,
  SidebarMenu,
  SidebarMenuBadge,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarProvider,
  SidebarRail,
  SidebarTrigger,
} from "@/components/ui/sidebar"
import { PublicLinkFooter } from "@/components/public-link"
import { api, errorText, type PortalState, type PublicLink } from "@/lib/api"

type View = "users" | "apps" | "activity" | "settings"

export type ViewProps = { data: PortalState; reload: () => Promise<void> }

const TITLES: Record<View, string> = { users: "Users", apps: "Apps", activity: "Activity", settings: "Settings" }

export function AppShell({
  data,
  status,
  reload,
  onLock,
}: ViewProps & { status: { address: string; public: PublicLink }; onLock: () => void }) {
  const [view, setView] = useState<View>("users")
  const [refreshing, setRefreshing] = useState(false)
  // Is this computer fully set up? Checked once when the app is opened.
  const [setupProblems, setSetupProblems] = useState(0)
  useEffect(() => {
    api<{ checks: { ok: boolean; fix: string }[] }>("GET", "/api/admin/setup")
      .then((r) => setSetupProblems(r.checks.filter((c) => !c.ok && c.fix).length))
      .catch(() => {})
  }, [view])

  const refresh = async () => {
    setRefreshing(true)
    try {
      await reload()
      toast.success("Up to date")
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setRefreshing(false)
    }
  }

  const connected = data.users.reduce((n, u) => n + u.sessions.length, 0)
  const nav: { id: View; label: string; icon: typeof UsersIcon; count?: number | string }[] = [
    { id: "users", label: "Users", icon: UsersIcon, count: connected ? `${connected} on` : data.users.length },
    { id: "apps", label: "Apps", icon: AppWindowIcon, count: data.apps.length },
    { id: "activity", label: "Activity", icon: HistoryIcon },
    { id: "settings", label: "Settings", icon: SettingsIcon },
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
                <div className="grid flex-1 text-left text-sm leading-tight">
                  <span className="truncate font-medium">Access Portal</span>
                  <span className="truncate text-xs text-muted-foreground">Admin on this computer</span>
                </div>
              </SidebarMenuButton>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarHeader>

        <SidebarContent>
          <SidebarGroup>
            <SidebarGroupLabel>Manage</SidebarGroupLabel>
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
                    {item.count !== undefined && <SidebarMenuBadge>{item.count}</SidebarMenuBadge>}
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>

        <SidebarFooter>
          <PublicLinkFooter link={status.public} local={status.address} />
        </SidebarFooter>
        <SidebarRail />
      </Sidebar>

      <SidebarInset className="min-w-0">
        <header className="flex h-14 shrink-0 items-center gap-2 border-b px-4">
          <SidebarTrigger className="-ml-1" />
          <Separator orientation="vertical" className="mr-2 data-vertical:h-4 data-vertical:self-center" />
          <h1 className="text-base font-medium">{TITLES[view]}</h1>
          <Button
            variant="ghost"
            size="sm"
            className="ml-auto"
            onClick={refresh}
            disabled={refreshing}
          >
            <RefreshCwIcon className={refreshing ? "animate-spin" : undefined} />
            Refresh
          </Button>
          <Button variant="ghost" size="sm" onClick={onLock} title="Lock the app (the portal keeps running)">
            <LockIcon />
            Lock
          </Button>
        </header>
        <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
          {setupProblems > 0 && view !== "settings" && (
            <div className="flex items-center gap-3 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm">
              <TriangleAlertIcon className="size-4 shrink-0 text-amber-600 dark:text-amber-400" />
              <span className="flex-1">
                This computer isn’t fully set up for the portal ({setupProblems} thing{setupProblems === 1 ? "" : "s"} to fix).
              </span>
              <Button size="sm" variant="outline" onClick={() => setView("settings")}>
                Set it up
              </Button>
            </div>
          )}
          {data.admin.default_password && view !== "settings" && (
            <div className="flex items-center gap-3 rounded-lg border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-sm">
              <TriangleAlertIcon className="size-4 shrink-0 text-amber-600 dark:text-amber-400" />
              <span className="flex-1">
                The app still uses the default sign-in (admin / admin). Anyone at this computer could open it.
              </span>
              <Button size="sm" variant="outline" onClick={() => setView("settings")}>
                Change it
              </Button>
            </div>
          )}
          {view === "users" && <UsersView data={data} reload={reload} />}
          {view === "apps" && <AppsView data={data} reload={reload} />}
          {view === "activity" && <ActivityView data={data} reload={reload} />}
          {view === "settings" && <SettingsView data={data} reload={reload} />}
        </div>
      </SidebarInset>
    </SidebarProvider>
  )
}
