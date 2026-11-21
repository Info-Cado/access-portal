import { useState, type ComponentType } from "react"
import {
  BellIcon,
  CopyIcon,
  DatabaseIcon,
  LaptopIcon,
  RefreshCwIcon,
  TimerIcon,
  WifiIcon,
  XIcon,
} from "lucide-react"
import { toast } from "sonner"

import type { SetupState, ViewProps } from "@/components/app-shell"
import { AlertsSection, BackupsSection, UpdatesSection } from "@/components/maintenance-sections"
import { SettingsGroup, SettingsNote, SettingsRow } from "@/components/settings-ui"
import { ComputerSection } from "@/components/setup-section"
import { Button } from "@/components/ui/button"
import { Dialog, DialogClose, DialogContent, DialogTitle } from "@/components/ui/dialog"
import { NativeSelect } from "@/components/ui/native-select"
import { Switch } from "@/components/ui/switch"
import { api, errorText, type NetworkReach, type SessionSettings } from "@/lib/api"
import { cn } from "@/lib/utils"

export type SettingsSection = "computer" | "network" | "sessions" | "alerts" | "backups" | "updates"

const SECTIONS: { id: SettingsSection; label: string; icon: ComponentType<{ className?: string }> }[] = [
  { id: "computer", label: "This computer", icon: LaptopIcon },
  { id: "network", label: "Network", icon: WifiIcon },
  { id: "sessions", label: "Sessions", icon: TimerIcon },
  { id: "alerts", label: "Phone alerts", icon: BellIcon },
  { id: "backups", label: "Backups", icon: DatabaseIcon },
  { id: "updates", label: "Updates", icon: RefreshCwIcon },
]

/** Settings, as a popup over the app: sections on the left, one section at a time on the right. */
export function SettingsDialog({
  open,
  onOpenChange,
  section,
  onSection,
  data,
  reload,
  setup,
}: ViewProps & {
  open: boolean
  onOpenChange: (open: boolean) => void
  section: SettingsSection
  onSection: (section: SettingsSection) => void
  setup: SetupState
}) {
  const current = SECTIONS.find((s) => s.id === section) ?? SECTIONS[0]
  const setupProblems = setup.checks?.filter((c) => !c.ok).length ?? 0

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        showCloseButton={false}
        aria-describedby={undefined}
        // Move no focus on open, so nothing shows a focus ring until the keyboard is used.
        onOpenAutoFocus={(e) => e.preventDefault()}
        style={{ outline: "none" }}
        className="flex h-[min(680px,calc(100svh-2rem))] w-[calc(100%-2rem)] max-w-none flex-col gap-0 overflow-hidden rounded-2xl p-0 sm:max-w-4xl sm:flex-row"
      >
        <nav className="flex shrink-0 flex-col gap-1 border-b bg-muted/40 p-2 sm:w-52 sm:border-r sm:border-b-0">
          <DialogTitle className="flex h-9 items-center px-2 text-sm font-medium">Settings</DialogTitle>
          <div className="flex gap-0.5 overflow-x-auto sm:flex-col">
            {SECTIONS.map((s) => (
              <button
                key={s.id}
                type="button"
                onClick={() => onSection(s.id)}
                aria-current={s.id === section ? "page" : undefined}
                className={cn(
                  "flex h-8 shrink-0 items-center gap-2.5 rounded-lg px-2 text-sm text-muted-foreground transition-colors outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/50",
                  s.id === section && "bg-background text-foreground shadow-xs ring-1 ring-border/60",
                )}
              >
                <s.icon className="size-4" />
                {s.label}
                {s.id === "computer" && setupProblems > 0 && (
                  <span className="ml-auto size-1.5 rounded-full bg-amber-500" aria-label={`${setupProblems} to fix`} />
                )}
              </button>
            ))}
          </div>
        </nav>

        <div className="flex min-h-0 min-w-0 flex-1 flex-col">
          <header className="flex h-12 shrink-0 items-center border-b pr-2 pl-6">
            <h2 className="text-sm font-medium">{current.label}</h2>
            <DialogClose asChild>
              <Button variant="ghost" size="icon-sm" className="ml-auto" aria-label="Close">
                <XIcon />
              </Button>
            </DialogClose>
          </header>
          <div className="min-h-0 flex-1 overflow-y-auto p-6">
            {section === "computer" && <ComputerSection setup={setup} />}
            {section === "network" && <NetworkSection network={data.network} reload={reload} />}
            {section === "sessions" && <SessionsSection settings={data.settings} reload={reload} />}
            {section === "alerts" && <AlertsSection data={data} reload={reload} />}
            {section === "backups" && <BackupsSection />}
            {section === "updates" && <UpdatesSection />}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}

const IDLE_CHOICES: [number, string][] = [
  [0, "Never"],
  [15, "After 15 minutes"],
  [30, "After 30 minutes"],
  [60, "After 1 hour"],
  [120, "After 2 hours"],
  [240, "After 4 hours"],
  [480, "After 8 hours"],
]

const SIGNOUT_CHOICES: [number, string][] = [
  [0, "Never"],
  [1, "After 1 hour"],
  [2, "After 2 hours"],
  [4, "After 4 hours"],
  [8, "After 8 hours"],
  [12, "After 12 hours"],
  [24, "After 1 day"],
  [72, "After 3 days"],
]

/** Keep a value that isn't one of the presets selectable. */
function withCurrent(choices: [number, string][], value: number, unit: string): [number, string][] {
  return choices.some(([v]) => v === value) ? choices : [...choices, [value, `After ${value} ${unit}`]]
}

/** Opening the portal straight from the local network or Tailscale, besides the public link. */
function NetworkSection({ network, reload }: { network: NetworkReach; reload: () => Promise<void> }) {
  const [busy, setBusy] = useState(false)

  const save = async (next: { lan: boolean; tailscale: boolean }, done: string) => {
    setBusy(true)
    try {
      await api("PUT", "/api/admin/settings/network", next)
      await reload()
      toast.success(done)
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  const lan = network.urls.filter((u) => u.kind === "lan")
  const tailscale = network.urls.filter((u) => u.kind === "tailscale")

  return (
    <>
      <SettingsGroup>
        <SettingsRow
          title="Local network"
          htmlFor="reach-lan"
          description={
            network.lan ? (
              <Addresses urls={lan} empty="This computer isn't on a local network now" />
            ) : (
              "Off. Office devices use the public link"
            )
          }
        >
          <Switch
            id="reach-lan"
            checked={network.lan}
            disabled={busy}
            onCheckedChange={(on) =>
              save({ lan: on, tailscale: network.tailscale }, on ? "Local network on" : "Local network off")
            }
          />
        </SettingsRow>
        <SettingsRow
          title="Tailscale"
          htmlFor="reach-tailscale"
          description={
            network.tailscale ? (
              <Addresses urls={tailscale} empty="Tailscale isn't running on this computer" />
            ) : (
              "Off. Tailscale devices use the public link"
            )
          }
        >
          <Switch
            id="reach-tailscale"
            checked={network.tailscale}
            disabled={busy}
            onCheckedChange={(on) => save({ lan: network.lan, tailscale: on }, on ? "Tailscale on" : "Tailscale off")}
          />
        </SettingsRow>
      </SettingsGroup>
      {(network.lan || network.tailscale) && (
        <SettingsNote>
          The first time, the browser says the connection isn’t private. Choose Advanced, then Continue.
        </SettingsNote>
      )}
    </>
  )
}

/** The addresses to type, each copied with a click. */
function Addresses({ urls, empty }: { urls: { url: string }[]; empty: string }) {
  if (urls.length === 0) return <>{empty}</>
  const copy = (url: string) =>
    navigator.clipboard
      .writeText(url)
      .then(() => toast.success("Address copied", { description: url }))
      .catch(() => toast.error("Could not copy. Select the address and copy it by hand."))
  return (
    <span className="flex flex-wrap gap-x-3 gap-y-1">
      {urls.map((u) => (
        <button
          key={u.url}
          type="button"
          title="Copy"
          onClick={() => copy(u.url)}
          className="inline-flex items-center gap-1.5 font-mono text-xs text-muted-foreground transition-colors hover:text-foreground"
        >
          {u.url.replace(/^https?:\/\//, "")}
          <CopyIcon className="size-3" />
        </button>
      ))}
    </span>
  )
}

function SessionsSection({ settings, reload }: { settings: SessionSettings; reload: () => Promise<void> }) {
  const [busy, setBusy] = useState(false)

  const save = async (next: SessionSettings, done: string) => {
    setBusy(true)
    try {
      await api("PUT", "/api/admin/settings/sessions", next)
      await reload()
      toast.success(done)
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <>
      <SettingsGroup>
        <SettingsRow
          title="Disconnect when idle"
          htmlFor="idle"
          description="No keyboard or mouse for this long. Apps keep running; a warning comes a minute before"
        >
          <NativeSelect
            id="idle"
            className="w-44"
            value={settings.idle_minutes}
            disabled={busy}
            onChange={(e) => save({ ...settings, idle_minutes: Number(e.target.value) }, "Idle setting saved")}
          >
            {withCurrent(IDLE_CHOICES, settings.idle_minutes, "minutes").map(([v, label]) => (
              <option key={v} value={v}>
                {label}
              </option>
            ))}
          </NativeSelect>
        </SettingsRow>
        <SettingsRow
          title="Sign out when disconnected"
          htmlFor="signout"
          description="Apps close and unsaved work is lost. Administrators are never signed out"
        >
          <NativeSelect
            id="signout"
            className="w-44"
            value={settings.signout_hours}
            disabled={busy}
            onChange={(e) => save({ ...settings, signout_hours: Number(e.target.value) }, "Sign-out setting saved")}
          >
            {withCurrent(SIGNOUT_CHOICES, settings.signout_hours, "hours").map(([v, label]) => (
              <option key={v} value={v}>
                {label}
              </option>
            ))}
          </NativeSelect>
        </SettingsRow>
        <SettingsRow
          title="Ask before viewing a screen"
          htmlFor="consent"
          description={
            settings.shadow_consent
              ? "Windows asks the user each time"
              : "Viewing starts at once, with nothing shown to the user"
          }
        >
          <Switch
            id="consent"
            checked={settings.shadow_consent}
            disabled={busy}
            onCheckedChange={(on) =>
              save({ ...settings, shadow_consent: on }, on ? "Users will be asked first" : "Users won't be asked")
            }
          />
        </SettingsRow>
      </SettingsGroup>
      <SettingsNote>Changes apply to sessions opened from now on</SettingsNote>
    </>
  )
}
