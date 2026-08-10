export class ApiFailure extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message)
  }
}

/// How long a control-plane call may take before it is abandoned.
///
/// Without a deadline a stalled connection - a proxy holding the socket, a
/// server that accepted and never answered - leaves the promise pending
/// forever, and the UI shows a spinner that never resolves and never errors.
/// That is indistinguishable to a user from the request still working.
const DEFAULT_TIMEOUT_MS = 30_000

/// Analytical queries are the one call that is legitimately slow, so they get
/// their own ceiling rather than forcing the control-plane deadline up to
/// match. Kept above the server's own execution deadline so a slow query
/// surfaces the server's error rather than a client-side abort that says
/// nothing about why.
const QUERY_TIMEOUT_MS = 120_000

export interface RequestOptions extends RequestInit {
  /// Overrides the deadline for this call. Zero disables it.
  timeoutMs?: number
}

export function usePintailApi() {
  const token = useState<string | null>('pintail-token', () => null)

  function restoreToken() {
    if (import.meta.client) {
      token.value = window.localStorage.getItem('pintail.token')
    }
  }

  function setToken(value: string | null) {
    token.value = value
    if (import.meta.client) {
      if (value) window.localStorage.setItem('pintail.token', value)
      else window.localStorage.removeItem('pintail.token')
    }
  }

  async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
    const { timeoutMs, ...init } = options
    const headers = new Headers(init.headers)
    if (token.value) headers.set('Authorization', `Bearer ${token.value}`)
    if (init.body && !headers.has('Content-Type')) {
      headers.set('Content-Type', 'application/json')
    }
    const deadline = timeoutMs
      ?? (path.startsWith('/query') ? QUERY_TIMEOUT_MS : DEFAULT_TIMEOUT_MS)
    // An explicit signal from the caller wins: it is usually a cancellation
    // the user asked for, which must not be overridden by a deadline.
    const signal = init.signal
      ?? (deadline > 0 ? AbortSignal.timeout(deadline) : undefined)
    let response: Response
    try {
      response = await fetch(`/api${path}`, { ...init, headers, signal })
    } catch (failure) {
      // A timeout aborts rather than rejecting with a network error, and the
      // bare DOMException reads as "signal is aborted without reason".
      if (failure instanceof DOMException && failure.name === 'TimeoutError') {
        throw new ApiFailure(`Request timed out after ${deadline / 1000}s`, 0)
      }
      throw failure
    }
    if (!response.ok) {
      let message = `${response.status} ${response.statusText}`
      try {
        const body = (await response.json()) as { error?: string }
        message = body.error || message
      } catch {
        // Preserve the HTTP fallback when the server did not return JSON.
      }
      throw new ApiFailure(message, response.status)
    }
    if (response.status === 204) return undefined as T
    return (await response.json()) as T
  }

  return { token, restoreToken, setToken, request }
}
