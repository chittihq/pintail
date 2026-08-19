/// Live process vitals, streamed at one sample per second.
///
/// `EventSource` is not used, for the same reason the control-plane event
/// stream does not use it: it cannot send an `Authorization` header, and these
/// routes are authenticated. So the stream is read from `fetch` as a
/// `ReadableStream` and the SSE framing is parsed here.
export interface VitalsSample {
  cpu_percent: number
  memory_bytes: number
  memory_limit_bytes: number | null
  queries_per_second: number
  queries_total: number
  /// Client-assigned. The server sends no timestamp because the browser's
  /// clock is what the axis is drawn against.
  at: number
}

/// How many seconds stay on screen. At 1 Hz this is also the sample count.
const WINDOW_SECONDS = 60

/// Module scope, not per-composable-instance: `streaming` is shared state,
/// so the abort handle that pairs with it must be shared too - a second
/// component instance calling stop() has to end the stream the first one
/// started, not silently miss its own undefined handle.
let abort: AbortController | undefined

export function useVitals() {
  const { token } = usePintailApi()
  const samples = useState<VitalsSample[]>('pintail-vitals', () => [])
  const streaming = useState('pintail-vitals-streaming', () => false)

  function push(sample: Omit<VitalsSample, 'at'>) {
    samples.value = [...samples.value, { ...sample, at: Date.now() }].slice(-WINDOW_SECONDS)
  }

  async function start() {
    if (streaming.value) return
    abort?.abort()
    const controller = new AbortController()
    abort = controller
    streaming.value = true
    try {
      // Reconnect until stop(): the card sits on the overview for hours, and
      // one dropped connection used to freeze the graph for the whole visit.
      while (!controller.signal.aborted) {
        try {
          await consume(controller)
        } catch {
          // Dropped connection; retry below unless stop() aborted us.
        }
        if (controller.signal.aborted) break
        await new Promise((resolve) => setTimeout(resolve, 3000))
      }
    } finally {
      streaming.value = false
    }
  }

  async function consume(controller: AbortController) {
    const response = await fetch('/api/vitals', {
      headers: { Authorization: `Bearer ${token.value}` },
      signal: controller.signal,
    })
    const reader = response.body?.getReader()
    if (!response.ok || !reader) return
    const decoder = new TextDecoder()
    let buffered = ''
    for (;;) {
      const chunk = await reader.read()
      if (chunk.done) break
      buffered += decoder.decode(chunk.value, { stream: true })
      // SSE frames are separated by a blank line; a partial frame stays in
      // the buffer until the rest of it arrives.
      const frames = buffered.split('\n\n')
      buffered = frames.pop() ?? ''
      for (const frame of frames) {
        const data = frame
          .split('\n')
          .filter((line) => line.startsWith('data:'))
          .map((line) => line.slice(5).trim())
          .join('')
        if (!data) continue
        try {
          push(JSON.parse(data) as Omit<VitalsSample, 'at'>)
        } catch {
          // A malformed frame is skipped rather than ending the stream: one
          // bad sample should not cost the whole graph.
        }
      }
    }
  }

  function stop() {
    abort?.abort()
    abort = undefined
    streaming.value = false
  }

  return { samples, streaming, start, stop, WINDOW_SECONDS }
}
