// The building blocks of the Settings popup: a group of rows, each row a title
// and a short description on the left with its control on the right.

import type { ReactNode } from "react"
import { cn } from "@/lib/utils"

export function SettingsGroup({ className, children }: { className?: string; children: ReactNode }) {
  return <div className={cn("divide-y divide-border/60 rounded-xl border", className)}>{children}</div>
}

export function SettingsRow({
  title,
  description,
  htmlFor,
  children,
  className,
}: {
  title: ReactNode
  description?: ReactNode
  /** The control the title labels. */
  htmlFor?: string
  children?: ReactNode
  className?: string
}) {
  const Title = htmlFor ? "label" : "div"
  return (
    <div className={cn("flex min-h-14 flex-wrap items-center gap-x-4 gap-y-2 px-4 py-3", className)}>
      <div className="min-w-0 flex-1 basis-56">
        <Title htmlFor={htmlFor} className="block text-sm">
          {title}
        </Title>
        {description && <div className="mt-0.5 text-xs text-muted-foreground">{description}</div>}
      </div>
      {children && <div className="flex shrink-0 items-center gap-2">{children}</div>}
    </div>
  )
}

/** One line under a group. */
export function SettingsNote({ children, className }: { children: ReactNode; className?: string }) {
  return <p className={cn("mt-2 px-1 text-xs text-muted-foreground", className)}>{children}</p>
}
