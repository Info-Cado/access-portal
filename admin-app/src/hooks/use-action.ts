import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react"
import { toast } from "sonner"

import { errorText } from "@/lib/api"

/** Runs one action at a time and keeps the latest step it reported. */
export function useAction() {
  const [busy, setBusy] = useState(false)
  const [step, setStep] = useState("")

  const run = useCallback(async <T>(work: (onStep: (text: string) => void) => Promise<T>): Promise<T> => {
    setBusy(true)
    setStep("")
    try {
      return await work(setStep)
    } finally {
      setBusy(false)
      setStep("")
    }
  }, [])

  return { busy, step, run }
}

/**
 * A value shown at once when changed and saved in the background.
 *
 * Only one save runs at a time. Changes made while one runs are coalesced: when it
 * ends, the newest value is sent if it differs from what was saved. A failed save
 * puts back the last saved value and shows the error.
 *
 * `save` gets the value to save and the last saved one; it may answer the value the
 * portal settled on. The value from the portal (`server`) is taken over whenever no
 * change is waiting.
 */
export function useOptimisticValue<T>(
  server: T,
  save: (next: T, prev: T) => Promise<T | void>,
  equal: (a: T, b: T) => boolean = Object.is,
) {
  const [shown, setShown] = useState(server)
  const saved = useRef(server)
  const desired = useRef(server)
  const saving = useRef(false)
  const saveRef = useRef(save)
  useLayoutEffect(() => {
    saveRef.current = save
  })

  useEffect(() => {
    if (saving.current || !equal(desired.current, saved.current)) return
    if (!equal(server, saved.current)) {
      saved.current = server
      desired.current = server
      setShown(server)
    }
  }, [server, equal])

  const set = useCallback(
    (next: T) => {
      desired.current = next
      setShown(next)
      if (saving.current) return
      saving.current = true
      void (async () => {
        try {
          while (!equal(desired.current, saved.current)) {
            const sending = desired.current
            try {
              const answer = await saveRef.current(sending, saved.current)
              const settled = answer === undefined ? sending : answer
              saved.current = settled
              if (equal(desired.current, sending)) {
                desired.current = settled
                setShown(settled)
              }
            } catch (e) {
              toast.error(errorText(e))
              desired.current = saved.current
              setShown(saved.current)
              break
            }
          }
        } finally {
          saving.current = false
        }
      })()
    },
    [equal],
  )

  return [shown, set] as const
}
