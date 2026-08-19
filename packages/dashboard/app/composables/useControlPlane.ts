import { toast } from 'vue-sonner'
import { ApiFailure } from './usePintailApi'
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

/// Coalesces the SSE-triggered refresh. A burst of frames (a chunked copy
/// emits one per chunk) used to fire the full three-request refresh per
/// frame; one trailing refresh within two seconds carries the same
/// information. Module scope, not useState: timers cannot be serialized.
let liveRefreshTimer: ReturnType<typeof setTimeout> | undefined
let liveRefreshAt = 0

export function useControlPlane() {
  const { token, setToken, request } = usePintailApi()

  const session = useState<Session | null>('cp-session', () => null)
  const workspaces = useState<Workspace[]>('cp-workspaces', () => [])
  const nodeStatus = useState<NodeStatus | null>('cp-node-status', () => null)
  const databases = useState<DatabaseRecord[]>('cp-databases', () => [])
  const statuses = useState<Record<string, DatabaseStatus>>('cp-statuses', () => ({}))
  const activity = useState<ActivityRecord[]>('cp-activity', () => [])
  const deadLetters = useState<DlqRecord[]>('cp-dlq', () => [])
  /** Live copy progress per \`databaseId:table\`, parsed from the event
   *  stream's snapshot.progress / resnapshot.progress frames. \`startedAt\`
   *  anchors the ETA-derived completion fraction the progress bar renders. */
  const tableProgress = useState<Record<string, { rows: number; etaSeconds: number | null; startedAt: number; updatedAt: number }>>(
    'cp-table-progress',
    () => ({}),
  )
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
      if (!expiredSession(failure)) error.value = messageOf(failure)
    } finally {
      loading.value = false
    }
  }

  async function refreshStatuses() {
    // allSettled, not all: one database mid-restart used to reject the whole
    // batch and freeze every OTHER database's status at its last value.
    const settled = await Promise.allSettled(
      databases.value.map(async (database) => {
        const status = await request<DatabaseStatus>(`/databases/${database.id}/status`)
        return [database.id, status] as const
      }),
    )
    const fresh = Object.fromEntries(
      settled
        .filter((outcome): outcome is PromiseFulfilledResult<readonly [string, DatabaseStatus]> =>
          outcome.status === 'fulfilled')
        .map((outcome) => outcome.value),
    )
    statuses.value = { ...statuses.value, ...fresh }
    databases.value = databases.value.map(
      (database) => fresh[database.id]?.database ?? database,
    )
  }

  async function refreshLiveData() {
    // Each surface refreshes independently: a failing activity endpoint must
    // not also freeze the DLQ list and per-database statuses (or vice versa).
    const [activityRows, dlqRows] = await Promise.allSettled([
      request<ActivityRecord[]>('/activity?limit=200'),
      request<DlqRecord[]>('/dlq?limit=100'),
      refreshStatuses(),
    ])
    if (activityRows.status === 'fulfilled') activity.value = activityRows.value as ActivityRecord[]
    if (dlqRows.status === 'fulfilled') deadLetters.value = dlqRows.value as DlqRecord[]
  }

  function queueLiveRefresh() {
    const wait = liveRefreshAt + 2000 - Date.now()
    if (wait <= 0) {
      liveRefreshAt = Date.now()
      void refreshLiveData()
      return
    }
    if (liveRefreshTimer) return
    liveRefreshTimer = setTimeout(() => {
      liveRefreshTimer = undefined
      liveRefreshAt = Date.now()
      void refreshLiveData()
    }, wait)
  }

  /// Seeds the live map from the server's retained copy of the same frames,
  /// so a page that loads MID-copy (reload, second browser) draws the bar
  /// immediately. A fresh SSE entry always wins over the seed - the stream
  /// is the primary source and this only fills the gap before its next frame.
  function seedTableProgress(
    databaseId: string,
    tables: Array<{ name: string; progress: { rows: number; eta_seconds: number | null; elapsed_seconds: number } | null }>,
  ) {
    for (const table of tables) {
      if (!table.progress) continue
      const key = `${databaseId}:${table.name}`
      const live = tableProgress.value[key]
      if (live && Date.now() - live.updatedAt < 10_000) continue
      tableProgress.value = {
        ...tableProgress.value,
        [key]: {
          rows: table.progress.rows,
          etaSeconds: table.progress.eta_seconds,
          startedAt: Date.now() - table.progress.elapsed_seconds * 1000,
          updatedAt: Date.now(),
        },
      }
    }
  }

  function clearSessionState() {
    stopEventStream()
    setToken(null)
    session.value = null
    databases.value = []
    statuses.value = {}
    activity.value = []
    deadLetters.value = []
    tableProgress.value = {}
  }

  /// A 401 mid-session means the token died server-side (expiry, key
  /// rotation, workspace deletion). Without this the dashboard froze on its
  /// last data forever - every poll failing quietly, no path back to the
  /// sign-in form short of clearing localStorage by hand.
  function sessionExpired() {
    if (!session.value) return
    clearSessionState()
    toast('Session expired - sign in again')
  }

  function expiredSession(failure: unknown) {
    if (failure instanceof ApiFailure && failure.status === 401 && session.value) {
      sessionExpired()
      return true
    }
    return false
  }

  async function startEventStream() {
    eventAbort.value?.abort()
    const controller = new AbortController()
    eventAbort.value = controller
    let backoffMs = 2000
    // Reconnect until sign-out. A proxy closing the socket used to silently
    // downgrade the dashboard to the 8s poll for the rest of the session.
    while (session.value && !controller.signal.aborted) {
      const connectedAt = Date.now()
      try {
        await consumeEventStream(controller)
      } catch {
        // Dropped connection or abort; the loop below decides which.
      }
      if (!session.value || controller.signal.aborted) return
      // A connection that held for a while earns a fresh, fast retry; rapid
      // failures back off so a down server is not hammered.
      backoffMs = Date.now() - connectedAt > 60_000 ? 2000 : Math.min(backoffMs * 2, 30_000)
      await new Promise((resolve) => setTimeout(resolve, backoffMs))
    }
  }

  async function consumeEventStream(controller: AbortController) {
    const response = await fetch('/api/events', {
      headers: { Authorization: `Bearer ${token.value}` },
      signal: controller.signal,
    })
    if (response.status === 401) {
      sessionExpired()
      return
    }
    const reader = response.body?.getReader()
    if (!response.ok || !reader) return
    const decoder = new TextDecoder()
    let buffered = ''
    while (session.value && !controller.signal.aborted) {
        const chunk = await reader.read()
        if (chunk.done) break
        buffered += decoder.decode(chunk.value, { stream: true })
        if (buffered.includes('\n\n')) {
          const complete = buffered.slice(0, buffered.lastIndexOf('\n\n'))
          buffered = buffered.slice(buffered.lastIndexOf('\n\n') + 2)
          for (const frame of complete.split('\n\n')) {
            const data = frame.split('\n').find((line) => line.startsWith('data: '))
            if (!data) continue
            try {
              const event = JSON.parse(data.slice(6)) as {
                kind: string
                database_id?: string
                table?: string
                rows?: number
                eta_seconds?: number | null
              }
              if (!event.database_id || !event.table) continue
              const key = `${event.database_id}:${event.table}`
              if (event.kind === 'resnapshot.progress' || event.kind === 'snapshot.progress') {
                // An entry whose updates stopped long ago is a leftover from
                // a run whose completion frame was missed; reusing its
                // startedAt would make the new run's bar start near-full.
                const previous = tableProgress.value[key]
                const existing = previous && Date.now() - previous.updatedAt < 30_000 ? previous : undefined
                tableProgress.value = {
                  ...tableProgress.value,
                  [key]: {
                    rows: event.rows ?? existing?.rows ?? 0,
                    etaSeconds: event.eta_seconds ?? null,
                    startedAt: existing?.startedAt ?? Date.now(),
                    updatedAt: Date.now(),
                  },
                }
              } else if (
                event.kind === 'resnapshot.completed'
                || event.kind === 'snapshot.completed'
                || event.kind === 'resnapshot.error'
                || event.kind === 'resnapshot.interrupted'
              ) {
                const { [key]: _finished, ...rest } = tableProgress.value
                tableProgress.value = rest
              }
            } catch {
              // A malformed frame only skips its own parse; refresh still runs.
            }
          }
          queueLiveRefresh()
        }
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
    // The empty caches below are indistinguishable from a workspace with no
    // databases, and the pages key their empty states on exactly that - so
    // the whole span from here to the reload completing must read as
    // loading, or the connection wizard flashes on every switch.
    loading.value = true
    const previousToken = token.value
    try {
      setToken(response.token)
      try {
        session.value = await request<Session>('/session')
      } catch (failure) {
        // The new token never proved itself, so keep the credential that
        // was working - otherwise a failed switch strands the operator
        // signed out of BOTH workspaces.
        setToken(previousToken)
        throw failure
      }
      databases.value = []
      statuses.value = {}
      activity.value = []
      deadLetters.value = []
      tableProgress.value = {}
      await loadWorkspaces()
      await loadControlPlane()
    } finally {
      loading.value = false
    }
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
      return true
    } catch (failure) {
      error.value = messageOf(failure)
      toast(`Workspace switch failed: ${messageOf(failure)}`)
      return false
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
    clearSessionState()
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
  /// Every mutation shares one contract: retry the supervisor's busy
  /// window (the job slot is held through every replication cycle, so
  /// first clicks frequently land on a transient 409), toast the failure
  /// loudly if it persists, and report success so callers can gate their
  /// follow-up work. It returns a boolean rather than throwing because
  /// most callers are template @click handlers, where a rethrow is an
  /// unhandled rejection, not a signal. The alternative was seven buttons
  /// that looked dead - their failures went to a page-level banner nobody
  /// watches.
  async function mutate(label: string, action: () => Promise<unknown>): Promise<boolean> {
    try {
      for (let attempt = 0; ; attempt += 1) {
        try {
          await action()
          return true
        } catch (failure) {
          const busy = failure instanceof ApiFailure && failure.status === 409
          if (!busy || attempt >= 14) throw failure
          await new Promise((resolve) => setTimeout(resolve, 2000))
        }
      }
    } catch (failure) {
      if (!expiredSession(failure)) {
        error.value = messageOf(failure)
        toast(`${label} failed: ${messageOf(failure)}`)
      }
      return false
    }
  }

  async function setReconcileInterval(database: DatabaseRecord, seconds: number) {
    const done = await mutate(`Setting the reconcile interval`, () =>
      request(`/databases/${database.id}`, {
        method: 'PUT',
        body: JSON.stringify({
          name: database.name,
          mode: database.mode,
          poll_interval_seconds: database.poll_interval_seconds,
          reconcile_interval_seconds: seconds,
        }),
      }))
    if (!done) return false
    toast(`Reconcile interval set to ${seconds}s`)
    await loadControlPlane()
    return true
  }

  async function setMode(database: DatabaseRecord, mode: DatabaseRecord['mode']) {
    {
      const done = await mutate('Changing the replication mode', () =>
        request(`/databases/${database.id}/mode`, {
          method: 'POST',
          body: JSON.stringify({ mode }),
        }))
      if (!done) return
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
    }
  }

  async function forceSnapshot(databaseId: string) {
    const done = await mutate('Resnapshot', () =>
      request(`/databases/${databaseId}/snapshot`, {
        method: 'POST',
        body: JSON.stringify({ force: true }),
      }))
    if (done) toast('Resnapshot accepted')
    return done
  }

  async function runTableAction(databaseId: string, table: TableSummary, action: 'resync' | 'reconcile') {
    // The supervisor holds the per-database job lock for the whole of every
    // replication cycle, so a click frequently lands on a 409 that clears
    // itself within seconds. Retrying briefly is the e2e harness's codified
    // behavior for this endpoint; without it the click died silently into a
    // page-level error nobody was looking at, and Resync read as
    // unresponsive. Anything still failing after the window toasts loudly.
    try {
      for (let attempt = 0; ; attempt += 1) {
        try {
          await request(
            `/databases/${encodeURIComponent(databaseId)}/tables/${encodeURIComponent(table.name)}/${action}`,
            { method: 'POST' },
          )
          break
        } catch (failure) {
          const busy = failure instanceof ApiFailure && failure.status === 409
          if (!busy || attempt >= 14) throw failure
          await new Promise((resolve) => setTimeout(resolve, 2000))
        }
      }
      if (action === 'resync') {
        toast(`${table.name} resnapshot accepted; other tables keep replicating`)
      } else {
        toast(`${table.name} reconciliation accepted`)
      }
    } catch (failure) {
      error.value = messageOf(failure)
      toast(`${table.name} ${action} failed: ${messageOf(failure)}`)
      throw failure
    }
  }

  async function resetDatabase(databaseId: string) {
    const done = await mutate('Reset', () =>
      request(`/databases/${databaseId}/reset`, { method: 'POST' }))
    if (!done) return false
    toast('Mirror reset; a fresh snapshot is running')
    await loadControlPlane()
    return true
  }

  async function removeDatabase(databaseId: string) {
    const done = await mutate('Removing the database', () =>
      request(`/databases/${databaseId}`, { method: 'DELETE' }))
    if (!done) return false
    toast('Database configuration removed; mirrored files were retained')
    await loadControlPlane()
    return true
  }

  async function discardDlq(record: DlqRecord) {
    const done = await mutate('Discarding the dead letter', () =>
      request(`/dlq/${record.id}`, { method: 'DELETE' }))
    if (!done) return
    // Discarding drops a row permanently, so it confirms like every other
    // mutation here. The row vanishing is not confirmation on its own - a
    // failed request leaves the identical screen behind.
    toast(`${record.table || 'Database'} dead letter discarded`)
    await refreshLiveData()
  }

  async function retryDlq(record: DlqRecord) {
    const done = await mutate('Retrying the dead letter', () =>
      request(`/dlq/${record.id}/retry`, { method: 'POST' }))
    if (!done) return
    toast(`${record.table || 'Database'} recovered; dead letter cleared`)
    await refreshLiveData()
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
    tableProgress,
    seedTableProgress,
    resetDatabase,
    removeDatabase,
    discardDlq,
    retryDlq,
    toggleTheme,
    applyTheme,
  }
}
