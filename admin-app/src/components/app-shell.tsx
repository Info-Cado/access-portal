import { useState } from "react"
import {
  AppWindowIcon,
  ChevronsUpDownIcon,
  LogOutIcon,
  MonitorSmartphoneIcon,
  RefreshCwIcon,
  UsersIcon,
} from "lucide-react"
import { toast } from "sonner"

import { AppsView } from "@/components/apps-view"
import { UsersView } from "@/components/users-view"
import { Avatar, AvatarFallback } from "@/components/ui/avatar"
import { Button } from "@/components/ui/button"
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
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
import { errorText, type PortalState } from "@/lib/api"

type View = "users" | "apps"

export type ViewProps = { data: PortalState; reload: () => Promise<void> }

export function AppShell({
  data,
  server,
  reload,
  onSignOut,
}: ViewProps & { server: string; onSignOut: () => void }) {
  const [view, setView] = useState<View>("users")
  const [refreshing, setRefreshing] = useState(false)
  const host = server.replace(/^https?:\/\//, "")

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

  const nav = [
    { id: "users" as const, label: "Users", icon: UsersIcon, count: data.users.length },
    { id: "apps" as const, label: "Apps", icon: AppWindowIcon, count: data.apps.length },
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
                  <span className="truncate text-xs text-muted-foreground">{host}</span>
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
                    <SidebarMenuBadge>{item.count}</SidebarMenuBadge>
                  </SidebarMenuItem>
                ))}
              </SidebarMenu>
            </SidebarGroupContent>
          </SidebarGroup>
        </SidebarContent>

        <SidebarFooter>
          <SidebarMenu>
            <SidebarMenuItem>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <SidebarMenuButton size="lg">
                    <Avatar className="size-8 rounded-lg">
                      <AvatarFallback className="rounded-lg">
                        {data.me.username.slice(0, 2).toUpperCase()}
                      </AvatarFallback>
                    </Avatar>
                    <div className="grid flex-1 text-left text-sm leading-tight">
                      <span className="truncate font-medium">{data.me.username}</span>
                      <span className="truncate text-xs text-muted-foreground">Administrator</span>
                    </div>
                    <ChevronsUpDownIcon className="ml-auto size-4" />
                  </SidebarMenuButton>
                </DropdownMenuTrigger>
                <DropdownMenuContent side="top" align="start" className="min-w-52">
                  <DropdownMenuLabel className="font-normal text-muted-foreground">
                    Signed in to {host}
                  </DropdownMenuLabel>
                  <DropdownMenuSeparator />
                  <DropdownMenuItem onSelect={onSignOut}>
                    <LogOutIcon />
                    Sign out
                  </DropdownMenuItem>
                </DropdownMenuContent>
              </DropdownMenu>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarFooter>
        <SidebarRail />
      </Sidebar>

      <SidebarInset className="min-w-0">
        <header className="flex h-14 shrink-0 items-center gap-2 border-b px-4">
          <SidebarTrigger className="-ml-1" />
          <Separator orientation="vertical" className="mr-2 data-vertical:h-4 data-vertical:self-center" />
          <h1 className="text-base font-medium">{view === "users" ? "Users" : "Apps"}</h1>
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
        </header>
        <div className="flex flex-1 flex-col gap-4 p-4 lg:p-6">
          {view === "users" ? (
            <UsersView data={data} reload={reload} />
          ) : (
            <AppsView data={data} reload={reload} />
          )}
        </div>
      </SidebarInset>
    </SidebarProvider>
  )
}
