import { toast } from 'vue-sonner'
import { messageOf } from '@/lib/format'
import type {
  ActivityRecord,
  DatabaseRecord,
  DatabaseStatus,
  DlqRecord,
  Session,
  TableSummary,
  Workspace,
} from '@/types/pintail'

export type NodeStatus = {
  status: string
  version: string
  wire: {
    enabled: boolean
    bind: string | null
    host: string | null
    port: number | null
    read_only: boolean
    authentication: string
  }
}

export function useControlPlane() {
  const { token, setToken, request } = usePintailApi()

  const session = useState<Session | null>('cp-session', () => null)
  const workspaces = useState<Workspace[]>('cp-workspaces', () => [])
  const nodeStatus = useState<NodeStatus | null>('cp-node-status', () => null)
  const databases = useState<DatabaseRecord[]>('cp-databases', () => [])
  const statuses = useState<Record<string, DatabaseStatus>>('cp-statuses', () => ({}))
  const activity = useState<ActivityRecord[]>('cp-activity', () => [])
  const deadLetters = useState<DlqRecord[]>('cp-dlq', () => [])
  const loading = useState('cp-loading', () => false)
  const error = useState('cp-error', () => '')
  const dark = useState('cp-dark', () => false)
  const eventAbort = useState<AbortController | undefined>('cp-event-abort', () => undefined)

  const totalRows = computed(() =>
    Object.values(statuses.value).reduce((sum, status) => sum + status.rows, 0),
  )
  const activeMirrors = computed(
    () =>
      databases.value.filter((database) =>
        ['streaming', 'polling'].includes(database.state),
      ).length,
  )
  const alertCount = computed(
    () =>
      deadLetters.value.length +
      databases.value.filter((database) => database.state === 'needs_resync').length,
  )

  async function loadNodeStatus() {
    try {
      const response = await fetch('/status')
      if (!response.ok) return
      nodeStatus.value = (await response.json()) as NodeStatus
    } catch {
      // Connection help remains editable when runtime discovery is unavailable.
    }
  }

  async function loadControlPlane() {
    loading.value = true
    error.value = ''
    try {
      const [databaseRows, activityRows, dlqRows] = await Promise.all([
        request<DatabaseRecord[]>('/databases'),
        request<ActivityRecord[]>('/activity?limit=200'),
        request<DlqRecord[]>('/dlq?limit=100'),
      ])
      databases.value = databaseRows
      activity.value = activityRows
      deadLetters.value = dlqRows
      await refreshStatuses()
    } catch (failure) {
      error.value = messageOf(failure)
    } finally {
      loading.value = false
    }
  }

  async function refreshStatuses() {
    const pairs = await Promise.all(
      databases.value.map(async (database) => {
        const status = await request<DatabaseStatus>(`/databases/${database.id}/status`)
        return [database.id, status] as const
      }),
    )
    statuses.value = Object.fromEntries(pairs)
    databases.value = databases.value.map(
      (database) => statuses.value[database.id]?.database ?? database,
    )
  }

  async function refreshLiveData() {
    try {
      const [activityRows, dlqRows] = await Promise.all([
        request<ActivityRecord[]>('/activity?limit=200'),
        request<DlqRecord[]>('/dlq?limit=100'),
        refreshStatuses(),
      ])
      activity.value = activityRows
      deadLetters.value = dlqRows
    } catch {
      // Keep the last coherent live view; the top-level health indicator shows staleness.
    }
  }

  async function startEventStream() {
    eventAbort.value?.abort()
    eventAbort.value = new AbortController()
    try {
      const response = await fetch('/api/events', {
        headers: { Authorization: `Bearer ${token.value}` },
        signal: eventAbort.value.signal,
      })
      const reader = response.body?.getReader()
      if (!response.ok || !reader) return
      const decoder = new TextDecoder()
      let buffered = ''
      while (session.value) {
        const chunk = await reader.read()
        if (chunk.done) break
        buffered += decoder.decode(chunk.value, { stream: true })
        if (buffered.includes('\n\n')) {
          buffered = buffered.slice(buffered.lastIndexOf('\n\n') + 2)
          await refreshLiveData()
        }
      }
    } catch {
      // The timed refresh remains the fallback if a proxy closes SSE.
    }
  }

  function stopEventStream() {
    eventAbort.value?.abort()
    eventAbort.value = undefined
  }

  async function loadWorkspaces() {
    try {
      workspaces.value = await request<Workspace[]>('/workspaces')
    } catch (failure) {
      error.value = messageOf(failure)
    }
  }

  async function enterWorkspace(response: { token: string, workspace: Workspace }) {
    stopEventStream()
    setToken(response.token)
    session.value = await request<Session>('/session')
    databases.value = []
    statuses.value = {}
    activity.value = []
    deadLetters.value = []
    await loadWorkspaces()
    await loadControlPlane()
    // Deliberately not awaited: startEventStream consumes an SSE stream in a
    // loop that only ends when the session does, so awaiting it never
    // returns. Doing so left every caller hanging - the workspace was
    // created and the request succeeded, but the dialog never closed and its
    // spinner never stopped. app.vue calls it the same way.
    void startEventStream()
  }

  async function switchWorkspace(workspaceId: string) {
    try {
      const response = await request<{ token: string, workspace: Workspace }>(
        `/workspaces/${workspaceId}/switch`,
        { method: 'POST' },
      )
      await enterWorkspace(response)
      toast(`Switched to ${response.workspace.name}`)
    } catch (failure) {
      error.value = messageOf(failure)
    }
  }

  async function createWorkspace(name: string) {
    const response = await request<{ token: string, workspace: Workspace }>('/workspaces', {
      method: 'POST',
      body: JSON.stringify({ name }),
    })
    await enterWorkspace(response)
    toast(`${response.workspace.name} created`)
    return response.workspace
  }

  function logout() {
    const { setToken } = usePintailApi()
    stopEventStream()
    setToken(null)
    session.value = null
    databases.value = []
    statuses.value = {}
    activity.value = []
    deadLetters.value = []
  }

  /// Changes how often a database reconciles, without touching anything else.
  ///
  /// This is the only control an operator has over how quickly rows removed by
  /// a foreign-key cascade converge: MySQL applies CASCADE and SET NULL inside
  /// InnoDB without writing row events, so those rows never arrive through CDC
  /// and wait for reconciliation. The interval was settable through the API and
  /// shown but never editable here, so in practice it was fixed at its default.
  ///
  /// The update endpoint replaces the record, and omitting the table lists used
  /// to clear them; they are omitted deliberately here now that omission means
  /// unchanged.
  async function setReconcileInterval(database: DatabaseRecord, seconds: number) {
    try {
      await request(`/databases/${database.id}`, {
        method: 'PUT',
        body: JSON.stringify({
          name: database.name,
          mode: database.mode,
          poll_interval_seconds: database.poll_interval_seconds,
          reconcile_interval_seconds: seconds,
        }),
      })
      toast(`Reconcile interval set to ${seconds}s`)
      await loadControlPlane()
    } catch (failure) {
      error.value = failure instanceof Error ? failure.message : String(failure)
    }
  }

  async function setMode(database: DatabaseRecord, mode: DatabaseRecord['mode']) {
    try {
      await request(`/databases/${database.id}/mode`, {
        method: 'POST',
        body: JSON.stringify({ mode }),
      })
      // Four modes reach this, not two. Reporting anything that is not
      // "paused" as "resumed" told an operator who picked Polling that
      // replication had resumed, which is both wrong and unfalsifiable from
      // the toast - the one signal confirming the click did what was asked.
      // Resuming is still described as resuming, because that is what leaving
      // "paused" for any running mode is, and it is how the button is labelled.
      const resuming = database.mode === 'paused' && mode !== 'paused'
      toast(
        mode === 'paused'
          ? 'Replication paused'
          : resuming
            ? 'Replication resumed'
            : `Replication mode set to ${mode === 'auto' ? 'auto' : mode.toUpperCase()}`,
      )
      await loadControlPlane()
    } catch (failure) {
      error.value = messageOf(failure)
    }
  }

  async function forceSnapshot(databaseId: string) {
    try {
      await request(`/databases/${databaseId}/snapshot`, {
        method: 'POST',
        body: JSON.stringify({ force: true }),
      })
      toast('Resnapshot accepted')
    } catch (failure) {
      error.value = messageOf(failure)
    }
  }

  async function runTableAction(databaseId: string, table: TableSummary, action: 'resync' | 'reconcile') {
    try {
      await request(
        `/databases/${encodeURIComponent(databaseId)}/tables/${encodeURIComponent(table.name)}/${action}`,
        { method: 'POST' },
      )
      if (action === 'resync') {
        toast('Safe mirror-wide resnapshot accepted; tables share one source checkpoint')
      } else {
        toast(`${table.name} reconciliation accepted`)
      }
    } catch (failure) {
      error.value = messageOf(failure)
      throw failure
    }
  }

  async function removeDatabase(databaseId: string) {
    try {
      await request(`/databases/${databaseId}`, { method: 'DELETE' })
      toast('Database configuration removed; mirrored files were retained')
      await loadControlPlane()
    } catch (failure) {
      error.value = messageOf(failure)
    }
  }

  async function discardDlq(record: DlqRecord) {
    try {
      await request(`/dlq/${record.id}`, { method: 'DELETE' })
      // Discarding drops a row permanently, so it confirms like every other
      // mutation here. The row vanishing is not confirmation on its own - a
      // failed request leaves the identical screen behind.
      toast(`${record.table || 'Database'} dead letter discarded`)
      await refreshLiveData()
    } catch (failure) {
      error.value = messageOf(failure)
    }
  }

  async function retryDlq(record: DlqRecord) {
    try {
      await request(`/dlq/${record.id}/retry`, { method: 'POST' })
      toast(`${record.table || 'Database'} recovered; dead letter cleared`)
      await refreshLiveData()
    } catch (failure) {
      error.value = messageOf(failure)
    }
  }

  function applyTheme() {
    if (import.meta.client) document.documentElement.classList.toggle('dark', dark.value)
  }

  function toggleTheme() {
    dark.value = !dark.value
    if (import.meta.client) window.localStorage.setItem('pintail.theme', dark.value ? 'dark' : 'light')
    applyTheme()
  }

  return {
    session,
    workspaces,
    nodeStatus,
    databases,
    statuses,
    activity,
    deadLetters,
    loading,
    error,
    dark,
    totalRows,
    activeMirrors,
    alertCount,
    loadNodeStatus,
    loadWorkspaces,
    switchWorkspace,
    createWorkspace,
    loadControlPlane,
    refreshStatuses,
    refreshLiveData,
    startEventStream,
    stopEventStream,
    logout,
    setMode,
    setReconcileInterval,
    forceSnapshot,
    runTableAction,
    removeDatabase,
    discardDlq,
    retryDlq,
    toggleTheme,
    applyTheme,
  }
}
