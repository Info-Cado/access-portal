import { useState } from "react"
import { CheckIcon, ChevronsUpDownIcon, CopyIcon, ExternalLinkIcon } from "lucide-react"
import { openUrl } from "@tauri-apps/plugin-opener"
import { toast } from "sonner"

import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu"
import { SidebarMenu, SidebarMenuButton, SidebarMenuItem } from "@/components/ui/sidebar"
import type { PublicLink } from "@/lib/api"
import { cn } from "@/lib/utils"

const STATE: Record<PublicLink["state"], { label: string; dot: string; pulse?: boolean }> = {
  online: { label: "Online", dot: "bg-emerald-500", pulse: true },
  connecting: { label: "Connecting…", dot: "bg-amber-500" },
  "setting-up": { label: "Setting up…", dot: "bg-amber-500" },
  error: { label: "Problem", dot: "bg-destructive" },
  unavailable: { label: "No public link", dot: "bg-muted-foreground" },
}

/** Bottom-left of the sidebar: the address people use to reach this portal,
 * whether it is up, and Copy / Open. */
export function PublicLinkFooter({
  link,
  local,
  direct,
}: {
  link: PublicLink
  local: string
  /** Addresses on the local network and Tailscale. */
  direct: { kind: "lan" | "tailscale"; url: string }[]
}) {
  const [copied, setCopied] = useState(false)
  const state = STATE[link.state] ?? STATE.unavailable
  const host = link.url?.replace(/^https:\/\//, "")

  const copy = async () => {
    if (!link.url) return
    try {
      await navigator.clipboard.writeText(link.url)
      setCopied(true)
      toast.success("Link copied", { description: link.url })
      setTimeout(() => setCopied(false), 1500)
    } catch {
      toast.error("Could not copy. Select the link and copy it by hand.")
    }
  }

  return (
    <SidebarMenu>
      <SidebarMenuItem>
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <SidebarMenuButton size="lg" tooltip={host ?? state.label}>
              <span className="relative flex size-8 shrink-0 items-center justify-center">
                {state.pulse && <span className={cn("absolute size-2.5 animate-ping rounded-full opacity-60", state.dot)} />}
                <span className={cn("size-2.5 rounded-full", state.dot)} />
              </span>
              <div className="grid flex-1 text-left text-sm leading-tight">
                <span className="truncate font-medium">{host ?? "Public link"}</span>
                <span className="truncate text-xs text-muted-foreground">{state.label}</span>
              </div>
              <ChevronsUpDownIcon className="ml-auto size-4" />
            </SidebarMenuButton>
          </DropdownMenuTrigger>
          <DropdownMenuContent side="top" align="start" className="w-(--radix-dropdown-menu-trigger-width) min-w-64">
            <DropdownMenuLabel className="font-normal text-muted-foreground">
              {link.state === "online"
                ? "Share this link. Anyone with a portal account can sign in from any browser."
                : link.error || state.label}
            </DropdownMenuLabel>
            {link.url && (
              <>
                <DropdownMenuSeparator />
                <DropdownMenuItem onSelect={copy}>
                  {copied ? <CheckIcon /> : <CopyIcon />}
                  Copy link
                </DropdownMenuItem>
                <DropdownMenuItem onSelect={() => link.url && openUrl(link.url).catch((e) => toast.error(String(e)))}>
                  <ExternalLinkIcon />
                  Open in browser
                </DropdownMenuItem>
              </>
            )}
            {direct.length > 0 && (
              <>
                <DropdownMenuSeparator />
                <DropdownMenuLabel className="text-xs font-normal text-muted-foreground">
                  On the local network and Tailscale
                </DropdownMenuLabel>
                {direct.map((d) => (
                  <DropdownMenuItem
                    key={d.url}
                    onSelect={() =>
                      navigator.clipboard
                        .writeText(d.url)
                        .then(() => toast.success("Address copied", { description: d.url }))
                        .catch(() => toast.error("Could not copy."))
                    }
                  >
                    <CopyIcon />
                    <span className="truncate">{d.url.replace(/^http:\/\//, "")}</span>
                    <span className="ml-auto text-xs text-muted-foreground">{d.kind === "lan" ? "Local" : "Tailscale"}</span>
                  </DropdownMenuItem>
                ))}
              </>
            )}
            <DropdownMenuSeparator />
            <DropdownMenuLabel className="text-xs font-normal text-muted-foreground">
              Portal running on this computer at {local}. Closing this window keeps it running in the
              notification area.
            </DropdownMenuLabel>
          </DropdownMenuContent>
        </DropdownMenu>
      </SidebarMenuItem>
    </SidebarMenu>
  )
}
