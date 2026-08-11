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

export function useVitals() {
  const { token } = usePintailApi()
  const samples = useState<VitalsSample[]>('pintail-vitals', () => [])
  const streaming = useState('pintail-vitals-streaming', () => false)
  let abort: AbortController | undefined

  function push(sample: Omit<VitalsSample, 'at'>) {
    samples.value = [...samples.value, { ...sample, at: Date.now() }].slice(-WINDOW_SECONDS)
  }

  async function start() {
    if (streaming.value) return
    abort?.abort()
    abort = new AbortController()
    streaming.value = true
    try {
      const response = await fetch('/api/vitals', {
        headers: { Authorization: `Bearer ${token.value}` },
        signal: abort.signal,
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
    } catch {
      // Aborted on navigation, or the connection dropped. The card shows the
      // window it already has rather than clearing to empty.
    } finally {
      streaming.value = false
    }
  }

  function stop() {
    abort?.abort()
    abort = undefined
    streaming.value = false
  }

  return { samples, streaming, start, stop, WINDOW_SECONDS }
}
