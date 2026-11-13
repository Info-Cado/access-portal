// Small dialogs shared by the Users and Apps screens.

import { useEffect, useState } from "react"
import { CheckIcon, CopyIcon } from "lucide-react"
import { toast } from "sonner"

import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog"
import { Button } from "@/components/ui/button"
import { Checkbox } from "@/components/ui/checkbox"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Field, FieldGroup, FieldLabel } from "@/components/ui/field"
import { InputGroup, InputGroupAddon, InputGroupButton, InputGroupInput } from "@/components/ui/input-group"
import { errorText, type TotpSetup } from "@/lib/api"

export type Confirmation = {
  title: string
  description: string
  action: string
  /** An optional tick box shown above the buttons; its value goes to `run`. */
  option?: { label: string; checked: boolean }
  /** Does the work; the dialog closes when it succeeds. */
  run: (optionChecked: boolean) => Promise<void>
}

/** Asks before something destructive. Pass `null` to keep it closed. */
export function ConfirmDialog({
  confirmation,
  onClose,
}: {
  confirmation: Confirmation | null
  onClose: () => void
}) {
  const [busy, setBusy] = useState(false)
  const [ticked, setTicked] = useState(false)
  useEffect(() => setTicked(confirmation?.option?.checked ?? false), [confirmation])

  const go = async () => {
    if (!confirmation) return
    setBusy(true)
    try {
      await confirmation.run(ticked)
      onClose()
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <AlertDialog open={confirmation !== null} onOpenChange={(open) => !open && !busy && onClose()}>
      <AlertDialogContent>
        <AlertDialogHeader>
          <AlertDialogTitle>{confirmation?.title}</AlertDialogTitle>
          <AlertDialogDescription>{confirmation?.description}</AlertDialogDescription>
        </AlertDialogHeader>
        {confirmation?.option && (
          <Field orientation="horizontal">
            <Checkbox
              id="confirm-option"
              checked={ticked}
              disabled={busy}
              onCheckedChange={(v) => setTicked(v === true)}
            />
            <FieldLabel htmlFor="confirm-option" className="font-normal">
              {confirmation.option.label}
            </FieldLabel>
          </Field>
        )}
        <AlertDialogFooter>
          <AlertDialogCancel disabled={busy}>Cancel</AlertDialogCancel>
          <AlertDialogAction
            variant="destructive"
            disabled={busy}
            onClick={(ev) => {
              ev.preventDefault() // stay open until the work is done
              void go()
            }}
          >
            {confirmation?.action}
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  )
}

function CopyField({ id, label, value }: { id: string; label: string; value: string }) {
  const [copied, setCopied] = useState(false)

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value)
      setCopied(true)
      setTimeout(() => setCopied(false), 1500)
    } catch {
      toast.error("Could not copy. Select the text and copy it by hand.")
    }
  }

  return (
    <Field>
      <FieldLabel htmlFor={id}>{label}</FieldLabel>
      <InputGroup>
        <InputGroupInput id={id} value={value} readOnly className="font-mono text-xs" />
        <InputGroupAddon align="inline-end">
          <InputGroupButton size="icon-xs" aria-label={`Copy ${label.toLowerCase()}`} onClick={copy}>
            {copied ? <CheckIcon /> : <CopyIcon />}
          </InputGroupButton>
        </InputGroupAddon>
      </InputGroup>
    </Field>
  )
}

export type SecretToShow = { username: string; totp: TotpSetup }

/** Shows an authenticator setup key. The portal never shows it again, so this
 * only closes on purpose. */
export function SecretDialog({
  secret,
  onClose,
}: {
  secret: SecretToShow | null
  onClose: () => void
}) {
  return (
    <Dialog open={secret !== null}>
      <DialogContent
        showCloseButton={false}
        onEscapeKeyDown={(ev) => ev.preventDefault()}
        onInteractOutside={(ev) => ev.preventDefault()}
      >
        <DialogHeader>
          <DialogTitle>Set up {secret?.username}’s authenticator</DialogTitle>
          <DialogDescription>
            Add this key to an authenticator app now. It is not shown again; if it is lost, reset
            two-step verification for this user.
          </DialogDescription>
        </DialogHeader>
        {secret && (
          <FieldGroup>
            <CopyField id="totp-secret" label="Setup key" value={secret.totp.secret} />
            <CopyField id="totp-uri" label="Setup link" value={secret.totp.uri} />
          </FieldGroup>
        )}
        <DialogFooter>
          <Button onClick={onClose}>I’ve saved it</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
