import { setTimeout as sleep } from 'node:timers/promises'

/** Resolve Docker's current mapping on every poll: automatic restarts can
 * allocate a different ephemeral host port. Keep the original host/protocol. */
export async function waitForPublishedEndpoint(
  previousUrl: string,
  publishedPort: (signal: AbortSignal) => Promise<number>,
  ready: (url: string, signal: AbortSignal) => Promise<boolean>,
  { timeoutMs = 90_000, retryMs = 2_000 } = {},
): Promise<string> {
  const controller = new AbortController()
  const { signal } = controller
  let lastError: unknown
  let timer: ReturnType<typeof setTimeout>
  const expired = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      const error = new Error('Timed out waiting for the restarted benchmark endpoint', { cause: lastError })
      controller.abort(error)
      reject(error)
    }, timeoutMs)
  })
  const poll = async () => {
    while (true) {
      signal.throwIfAborted()
      try {
        const endpoint = new URL(previousUrl)
        endpoint.port = String(await publishedPort(signal))
        signal.throwIfAborted()
        const url = endpoint.origin
        const available = await ready(url, signal)
        signal.throwIfAborted()
        if (available) return url
      } catch (error) {
        signal.throwIfAborted()
        // The mapping may be absent while Docker restarts the container.
        lastError = error
      }
      await sleep(retryMs, undefined, { signal })
    }
  }
  try {
    // Race also bounds callbacks that do not honor cancellation. Production
    // callbacks use the signal to kill Docker lookups and cancel HTTP probes.
    return await Promise.race([poll(), expired])
  } finally {
    clearTimeout(timer!)
  }
}
