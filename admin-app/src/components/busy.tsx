// Feedback for actions that take a few seconds: a spinner and the latest step.

import type { ComponentProps, ReactNode } from "react"
import { LoaderCircleIcon } from "lucide-react"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import { useAction } from "@/hooks/use-action"
import { cn } from "@/lib/utils"
import { errorText } from "@/lib/api"

export function Spinner({ className }: { className?: string }) {
  return <LoaderCircleIcon className={cn("animate-spin", className)} />
}

/** A button's content while its action runs: a spinner and the latest step. */
export function BusyLabel({ step, fallback }: { step: string; fallback: ReactNode }) {
  return (
    <>
      <Spinner />
      <span className="truncate" title={step || undefined}>
        {step || fallback}
      </span>
    </>
  )
}

/** A button that runs an action; only it is disabled while the action runs.
 * Errors are shown as a toast. */
export function ActionButton({
  action,
  busyLabel,
  children,
  disabled,
  className,
  ...props
}: Omit<ComponentProps<typeof Button>, "onClick"> & {
  action: (onStep: (text: string) => void) => Promise<unknown>
  /** Shown until the first step arrives; defaults to the button's own text. */
  busyLabel?: ReactNode
}) {
  const { busy, step, run } = useAction()
  return (
    <Button
      {...props}
      className={cn("max-w-full", className)}
      disabled={disabled || busy}
      onClick={() => run(action).catch((e) => toast.error(errorText(e)))}
    >
      {busy ? <BusyLabel step={step} fallback={busyLabel ?? children} /> : children}
    </Button>
  )
}
