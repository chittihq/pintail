export class ApiFailure extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message)
  }
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

  async function request<T>(path: string, init: RequestInit = {}): Promise<T> {
    const headers = new Headers(init.headers)
    if (token.value) headers.set('Authorization', `Bearer ${token.value}`)
    if (init.body && !headers.has('Content-Type')) {
      headers.set('Content-Type', 'application/json')
    }
    const response = await fetch(`/api${path}`, { ...init, headers })
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
