import { useEffect, useState } from "react"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Field, FieldDescription, FieldLabel } from "@/components/ui/field"
import { Textarea } from "@/components/ui/textarea"
import { api, errorText } from "@/lib/api"

export type MessageTarget = {
  /** Who it goes to, for the dialog's title: "everyone connected", "anita". */
  label: string
  user_id?: number
  connection_id?: number
}

/** Write a message that pops up on top of users' sessions. */
export function MessageDialog({ target, onClose }: { target: MessageTarget | null; onClose: () => void }) {
  const [text, setText] = useState("")
  const [busy, setBusy] = useState(false)
  useEffect(() => {
    if (target) setText("")
  }, [target])

  const send = async (ev: React.FormEvent) => {
    ev.preventDefault()
    if (!target) return
    setBusy(true)
    try {
      const r = await api<{ sent: number }>("POST", "/api/admin/message", {
        text,
        user_id: target.user_id,
        connection_id: target.connection_id,
      })
      toast.success(r.sent === 1 ? "Message sent" : `Message sent to ${r.sent} sessions`)
      onClose()
    } catch (e) {
      toast.error(errorText(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <Dialog open={target !== null} onOpenChange={(open) => !open && onClose()}>
      <DialogContent>
        <form onSubmit={send} className="contents">
          <DialogHeader>
            <DialogTitle>Message {target?.label}</DialogTitle>
            <DialogDescription>
              It appears on top of their session until they click OK. Only people connected right now see it.
            </DialogDescription>
          </DialogHeader>
          <Field>
            <FieldLabel htmlFor="message-text">Message</FieldLabel>
            <Textarea
              id="message-text"
              rows={4}
              maxLength={2000}
              autoFocus
              placeholder="The computer restarts in 10 minutes. Please save your work."
              value={text}
              onChange={(e) => setText(e.target.value)}
            />
            <FieldDescription>{text.length} / 2000</FieldDescription>
          </Field>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={busy || !text.trim()}>
              Send
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
