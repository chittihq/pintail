<script setup lang="ts">
import {
  Activity,
  AlertTriangle,
  Archive,
  ArrowRight,
  Cable,
  Check,
  ChevronRight,
  CircleHelp,
  Copy,
  Database,
  HardDrive,
  KeyRound,
  LayoutDashboard,
  LoaderCircle,
  LogOut,
  Moon,
  Pause,
  Play,
  Plus,
  Radio,
  RefreshCw,
  Search,
  Server,
  Settings,
  SquareTerminal,
  Sun,
  Table2,
  Trash2,
  X,
} from '@lucide/vue'
import { useIntervalFn } from '@vueuse/core'
import { Badge } from '@/components/ui/badge'
import type {
  ActivityRecord,
  ApiKeyRecord,
  DatabaseRecord,
  DatabaseStatus,
  DlqRecord,
  ProbeReport,
  QueryResponse,
  Session,
  SnapshotStatus,
  TableSummary,
} from '@/types/pintail'

type Page =
  | 'overview'
  | 'databases'
  | 'database'
  | 'wizard'
  | 'sql'
  | 'activity'
  | 'keys'
  | 'backups'
  | 'settings'
  | 'connect'

type NodeStatus = {
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

const nav = [
  { id: 'overview' as Page, label: 'Overview', icon: LayoutDashboard },
  { id: 'databases' as Page, label: 'Databases', icon: Database },
  { id: 'sql' as Page, label: 'SQL Console', icon: SquareTerminal },
  { id: 'activity' as Page, label: 'Activity', icon: Activity },
  { id: 'keys' as Page, label: 'API Keys', icon: KeyRound },
  { id: 'backups' as Page, label: 'Backups', icon: Archive },
  { id: 'settings' as Page, label: 'Settings', icon: Settings },
  { id: 'connect' as Page, label: 'Connect', icon: Cable },
]

const { token, restoreToken, setToken, request } = usePintailApi()
const page = ref<Page>('overview')
const authMode = ref<'setup' | 'login'>('login')
const authenticating = ref(false)
const booting = ref(true)
const loading = ref(false)
const error = ref('')
const notice = ref('')
const dark = ref(false)
const session = ref<Session | null>(null)
const nodeStatus = ref<NodeStatus | null>(null)
const authForm = reactive({ email: '', password: '' })
const databases = ref<DatabaseRecord[]>([])
const statuses = ref<Record<string, DatabaseStatus>>({})
const activity = ref<ActivityRecord[]>([])
const deadLetters = ref<DlqRecord[]>([])
const selectedDatabaseId = ref('')
const tables = ref<TableSummary[]>([])
const snapshot = ref<SnapshotStatus | null>(null)
const detailTab = ref('tables')
const tableAction = ref('')
const deleteCandidate = ref<DatabaseRecord | null>(null)
const deleteText = ref('')

const wizard = reactive({
  step: 1,
  databaseId: '',
  name: '',
  dsn: '',
  serverVersion: '',
  mode: 'auto',
  probe: null as ProbeReport | null,
  includes: [] as string[],
  excludes: [] as string[],
  working: false,
  error: '',
})

const sqlDatabaseId = ref('')
const sqlText = ref('SELECT *\nFROM events\nLIMIT 100')
const sqlResult = ref<QueryResponse | null>(null)
const sqlRunning = ref(false)
const sqlError = ref('')
const activityDatabase = ref('')
const keyDatabaseId = ref('')
const keys = ref<ApiKeyRecord[]>([])
const keyForm = reactive({ name: '', scopes: ['read', 'query'] })
const revealedSecret = ref('')
const connectKey = ref('pk_your_key')
const connectHost = ref('127.0.0.1')
const connectPort = ref('3306')

const selectedDatabase = computed(
  () => databases.value.find((database) => database.id === selectedDatabaseId.value) ?? null,
)
const filteredActivity = computed(() =>
  activityDatabase.value
    ? activity.value.filter((record) => record.database_id === activityDatabase.value)
    : activity.value,
)
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
const selectedConnectDatabase = computed(
  () =>
    databases.value.find((database) => database.id === keyDatabaseId.value) ??
    databases.value[0] ??
    null,
)

useHead({
  bodyAttrs: { class: 'dashboard-body' },
})

onMounted(async () => {
  restoreToken()
  dark.value = window.localStorage.getItem('pintail.theme') === 'dark'
  applyTheme()
  await loadNodeStatus()
  try {
    const setup = await request<{ required: boolean }>('/auth/setup/status')
    authMode.value = setup.required ? 'setup' : 'login'
    if (token.value) {
      session.value = await request<Session>('/session')
      await loadControlPlane()
      startEventStream()
    }
  } catch {
    setToken(null)
  } finally {
    booting.value = false
  }
})

onBeforeUnmount(() => eventAbort?.abort())

useIntervalFn(
  () => {
    if (session.value && !loading.value) void refreshLiveData()
  },
  8_000,
)

async function submitAuth() {
  authenticating.value = true
  error.value = ''
  try {
    const response = await request<{
      token: string
      user: { id: string; email: string; role: string }
    }>(`/auth/${authMode.value}`, {
      method: 'POST',
      body: JSON.stringify(authForm),
    })
    setToken(response.token)
    session.value = await request<Session>('/session')
    await loadControlPlane()
    startEventStream()
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    authenticating.value = false
  }
}

async function loadNodeStatus() {
  try {
    const response = await fetch('/status')
    if (!response.ok) return
    nodeStatus.value = (await response.json()) as NodeStatus
    connectHost.value = window.location.hostname || '127.0.0.1'
    if (nodeStatus.value.wire.port) {
      connectPort.value = String(nodeStatus.value.wire.port)
    }
  } catch {
    // Connection help remains editable when runtime discovery is unavailable.
  }
}

function logout() {
  eventAbort?.abort()
  eventAbort = undefined
  setToken(null)
  session.value = null
  databases.value = []
  statuses.value = {}
  page.value = 'overview'
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
    const validIds = new Set(databaseRows.map((database) => database.id))
    const fallbackId = databaseRows[0]?.id || ''
    if (!validIds.has(selectedDatabaseId.value)) selectedDatabaseId.value = fallbackId
    if (!validIds.has(sqlDatabaseId.value)) sqlDatabaseId.value = fallbackId
    if (!validIds.has(keyDatabaseId.value)) keyDatabaseId.value = fallbackId
    if (activityDatabase.value && !validIds.has(activityDatabase.value)) {
      activityDatabase.value = ''
    }
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
    if (page.value === 'database' && selectedDatabaseId.value) {
      await loadDatabaseDetail(false)
    }
  } catch {
    // Keep the last coherent live view; the top-level health indicator shows staleness.
  }
}

let eventAbort: AbortController | undefined
async function startEventStream() {
  eventAbort?.abort()
  eventAbort = new AbortController()
  try {
    const response = await fetch('/api/events', {
      headers: { Authorization: `Bearer ${token.value}` },
      signal: eventAbort.signal,
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

function go(target: Page) {
  page.value = target
  error.value = ''
  if (target === 'keys') void loadKeys()
}

async function openDatabase(database: DatabaseRecord) {
  selectedDatabaseId.value = database.id
  page.value = 'database'
  detailTab.value = 'tables'
  await loadDatabaseDetail()
}

async function loadDatabaseDetail(showLoading = true) {
  if (!selectedDatabaseId.value) return
  if (showLoading) loading.value = true
  try {
    const [tableRows, snapshotStatus] = await Promise.all([
      request<TableSummary[]>(`/tables?db=${encodeURIComponent(selectedDatabaseId.value)}`),
      request<SnapshotStatus>(
        `/databases/${encodeURIComponent(selectedDatabaseId.value)}/snapshot/status`,
      ),
    ])
    tables.value = tableRows
    snapshot.value = snapshotStatus
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    if (showLoading) loading.value = false
  }
}

async function setMode(database: DatabaseRecord, mode: DatabaseRecord['mode']) {
  try {
    await request(`/databases/${database.id}/mode`, {
      method: 'POST',
      body: JSON.stringify({ mode }),
    })
    toast(mode === 'paused' ? 'Replication paused' : 'Replication resumed')
    await loadControlPlane()
    if (page.value === 'database') await loadDatabaseDetail()
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

async function forceSnapshot() {
  if (!selectedDatabase.value) return
  try {
    await request(`/databases/${selectedDatabase.value.id}/snapshot`, {
      method: 'POST',
      body: JSON.stringify({ force: true }),
    })
    detailTab.value = 'snapshot'
    toast('Resnapshot accepted')
    await loadDatabaseDetail()
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

async function runTableAction(table: TableSummary, action: 'resync' | 'reconcile') {
  if (!selectedDatabase.value) return
  tableAction.value = `${table.name}:${action}`
  error.value = ''
  try {
    await request(
      `/databases/${encodeURIComponent(selectedDatabase.value.id)}/tables/${encodeURIComponent(table.name)}/${action}`,
      { method: 'POST' },
    )
    if (action === 'resync') {
      detailTab.value = 'snapshot'
      toast('Safe mirror-wide resnapshot accepted; tables share one source checkpoint')
    } else {
      toast(`${table.name} reconciliation accepted`)
    }
    await loadDatabaseDetail()
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    tableAction.value = ''
  }
}

async function removeDatabase() {
  if (!deleteCandidate.value || deleteText.value !== deleteCandidate.value.name) return
  try {
    await request(`/databases/${deleteCandidate.value.id}`, { method: 'DELETE' })
    deleteCandidate.value = null
    deleteText.value = ''
    toast('Database configuration removed; mirrored files were retained')
    await loadControlPlane()
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

function beginWizard() {
  Object.assign(wizard, {
    step: 1,
    databaseId: '',
    name: '',
    dsn: '',
    serverVersion: '',
    mode: 'auto',
    probe: null,
    includes: [],
    excludes: [],
    working: false,
    error: '',
  })
  page.value = 'wizard'
}

async function wizardConnection() {
  wizard.working = true
  wizard.error = ''
  try {
    if (!wizard.databaseId) {
      const database = await request<DatabaseRecord>('/databases', {
        method: 'POST',
        body: JSON.stringify({
          name: wizard.name,
          dsn: wizard.dsn,
          mode: 'auto',
        }),
      })
      wizard.databaseId = database.id
    } else {
      await request(`/databases/${wizard.databaseId}`, {
        method: 'PUT',
        body: JSON.stringify({
          name: wizard.name,
          dsn: wizard.dsn,
          mode: 'auto',
          include_tables: [],
          exclude_tables: [],
          poll_interval_seconds: 5,
          reconcile_interval_seconds: 600,
        }),
      })
    }
    const tested = await request<{ ok: boolean; server_version: string }>(
      `/databases/${wizard.databaseId}/test`,
      { method: 'POST' },
    )
    wizard.serverVersion = tested.server_version
    wizard.step = 2
    wizard.probe = await request<ProbeReport>(`/databases/${wizard.databaseId}/probe`)
    wizard.mode = wizard.probe.capabilities.recommended_mode
    wizard.includes = wizard.probe.tables.map((table) => table.name)
  } catch (failure) {
    wizard.error = messageOf(failure)
  } finally {
    wizard.working = false
  }
}

async function finishWizard() {
  if (!wizard.probe) return
  wizard.working = true
  wizard.error = ''
  try {
    const allNames = wizard.probe.tables.map((table) => table.name)
    wizard.excludes = allNames.filter((name) => !wizard.includes.includes(name))
    await request(`/databases/${wizard.databaseId}`, {
      method: 'PUT',
      body: JSON.stringify({
        name: wizard.name,
        mode: wizard.mode,
        include_tables: wizard.includes,
        exclude_tables: wizard.excludes,
        poll_interval_seconds: 5,
        reconcile_interval_seconds: 600,
      }),
    })
    wizard.step = 4
    await request(`/databases/${wizard.databaseId}/snapshot`, {
      method: 'POST',
      body: JSON.stringify({ force: false }),
    })
    await loadControlPlane()
    const database = databases.value.find((item) => item.id === wizard.databaseId)
    if (database) await openDatabase(database)
    detailTab.value = 'snapshot'
    toast('Snapshot started')
  } catch (failure) {
    wizard.error = messageOf(failure)
  } finally {
    wizard.working = false
  }
}

async function runSql() {
  if (!sqlDatabaseId.value || !sqlText.value.trim()) return
  sqlRunning.value = true
  sqlError.value = ''
  try {
    sqlResult.value = await request<QueryResponse>('/query', {
      method: 'POST',
      body: JSON.stringify({ db: sqlDatabaseId.value, sql: sqlText.value }),
    })
    const history = JSON.parse(window.localStorage.getItem('pintail.sqlHistory') || '[]')
    window.localStorage.setItem(
      'pintail.sqlHistory',
      JSON.stringify([sqlText.value, ...history.filter((sql: string) => sql !== sqlText.value)].slice(0, 20)),
    )
  } catch (failure) {
    sqlError.value = messageOf(failure)
    sqlResult.value = null
  } finally {
    sqlRunning.value = false
  }
}

function exportResult(kind: 'json' | 'csv') {
  if (!sqlResult.value) return
  const fields = sqlResult.value.fields.map((field) => field.name)
  const content =
    kind === 'json'
      ? JSON.stringify(
          sqlResult.value.rows.map((row) => Object.fromEntries(fields.map((field, index) => [field, row[index]]))),
          null,
          2,
        )
      : [
          fields.map(csvCell).join(','),
          ...sqlResult.value.rows.map((row) => row.map(csvCell).join(',')),
        ].join('\n')
  const blob = new Blob([content], {
    type: kind === 'json' ? 'application/json' : 'text/csv',
  })
  const anchor = document.createElement('a')
  anchor.href = URL.createObjectURL(blob)
  anchor.download = `pintail-query.${kind}`
  anchor.click()
  URL.revokeObjectURL(anchor.href)
}

async function loadKeys() {
  if (!keyDatabaseId.value) {
    keys.value = []
    return
  }
  try {
    keys.value = await request<ApiKeyRecord[]>(
      `/databases/${keyDatabaseId.value}/api-keys`,
    )
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

async function createKey() {
  if (!keyDatabaseId.value || !keyForm.name.trim()) return
  try {
    const key = await request<ApiKeyRecord>(
      `/databases/${keyDatabaseId.value}/api-keys`,
      {
        method: 'POST',
        body: JSON.stringify({ name: keyForm.name, scopes: keyForm.scopes }),
      },
    )
    revealedSecret.value = key.secret || ''
    keyForm.name = ''
    await loadKeys()
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

async function toggleKey(key: ApiKeyRecord) {
  await request(`/databases/${key.database_id}/api-keys/${key.id}`, {
    method: 'PATCH',
    body: JSON.stringify({ enabled: !key.enabled }),
  })
  await loadKeys()
}

async function deleteKey(key: ApiKeyRecord) {
  await request(`/databases/${key.database_id}/api-keys/${key.id}`, {
    method: 'DELETE',
  })
  await loadKeys()
}

async function discardDlq(record: DlqRecord) {
  try {
    await request(`/dlq/${record.id}`, { method: 'DELETE' })
    await refreshLiveData()
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

function toggleTheme() {
  dark.value = !dark.value
  window.localStorage.setItem('pintail.theme', dark.value ? 'dark' : 'light')
  applyTheme()
}

function applyTheme() {
  document.documentElement.classList.toggle('dark', dark.value)
}

async function copy(value: string) {
  await navigator.clipboard.writeText(value)
  toast('Copied to clipboard')
}

function toast(message: string) {
  notice.value = message
  window.setTimeout(() => {
    if (notice.value === message) notice.value = ''
  }, 3_000)
}

function modeOf(database: DatabaseRecord) {
  return database.effective_mode || database.mode
}

function stateTone(state: string) {
  if (['streaming', 'completed', 'ready'].includes(state)) return 'positive'
  if (['polling', 'snapshotting', 'running', 'probed'].includes(state)) return 'warning'
  if (['error', 'needs_resync'].includes(state)) return 'negative'
  return 'neutral'
}

function formatNumber(value: number) {
  return new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 }).format(
    value,
  )
}

function formatBytes(value: number) {
  if (value < 1_024) return `${value} B`
  if (value < 1_048_576) return `${(value / 1_024).toFixed(1)} KiB`
  return `${(value / 1_048_576).toFixed(1)} MiB`
}

function formatDate(value: string | null) {
  return value ? new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(value)) : 'Never'
}

function displayValue(value: unknown) {
  if (value === null) return 'NULL'
  if (typeof value === 'object') return JSON.stringify(value)
  return String(value)
}

function messageOf(failure: unknown) {
  return failure instanceof Error ? failure.message : 'Unexpected control-plane error'
}

function csvCell(value: unknown) {
  const text = displayValue(value)
  return `"${text.replaceAll('"', '""')}"`
}

function snapshotPercent(table: SnapshotStatus['tables'][number]) {
  if (!table.total_chunks) return table.rows > 0 ? 100 : 0
  return Math.round((table.completed_chunks / table.total_chunks) * 100)
}

function connectSnippet(kind: 'mysql' | 'node' | 'python') {
  const database = selectedConnectDatabase.value?.name || 'analytics'
  const host = connectHost.value || '127.0.0.1'
  const port = Math.min(65_535, Math.max(1, Number.parseInt(connectPort.value, 10) || 3306))
  if (kind === 'node') {
    return `// bun add mysql2
import mysql from 'mysql2/promise'

const db = await mysql.createConnection({
  host: ${JSON.stringify(host)},
  port: ${port},
  user: ${JSON.stringify(database)},
  password: ${JSON.stringify(connectKey.value)},
  database: ${JSON.stringify(database)},
})
const [rows] = await db.query('SELECT * FROM events LIMIT 10')
console.table(rows)
await db.end()`
  }
  if (kind === 'python') {
    return `# uv run --with pymysql python connect.py
import pymysql

db = pymysql.connect(
    host=${JSON.stringify(host)},
    port=${port},
    user=${JSON.stringify(database)},
    password=${JSON.stringify(connectKey.value)},
    database=${JSON.stringify(database)},
)
with db.cursor() as cursor:
    cursor.execute("SELECT * FROM events LIMIT 10")
    print(cursor.fetchall())
db.close()`
  }
  return `MYSQL_PWD=${shellQuote(connectKey.value)} mysql \\
  --protocol=tcp \\
  --host=${shellQuote(host)} \\
  --port=${port} \\
  --user=${shellQuote(database)} \\
  --database=${shellQuote(database)}`
}

function shellQuote(value: string) {
  return `'${value.replaceAll("'", "'\"'\"'")}'`
}

function describeTable(table: TableSummary) {
  if (!selectedDatabase.value) return
  sqlText.value = `DESCRIBE \`${table.name.replaceAll('`', '``')}\``
  sqlDatabaseId.value = selectedDatabase.value.id
  go('sql')
}
</script>

<template>
  <div v-if="booting" class="boot-screen" aria-live="polite">
    <div class="brand-mark">PT</div>
    <LoaderCircle class="spin" :size="20" />
    <span>Opening control plane</span>
  </div>

  <main v-else-if="!session" class="auth-shell">
    <section class="auth-panel">
      <div class="auth-brand">
        <span class="brand-mark">PT</span>
        <span>Pintail</span>
      </div>
      <div>
        <p class="kicker">{{ authMode === 'setup' ? 'First boot' : 'Local control plane' }}</p>
        <h1>{{ authMode === 'setup' ? 'Create the operator.' : 'Welcome back.' }}</h1>
        <p class="muted auth-copy">
          {{
            authMode === 'setup'
              ? 'This one-time account owns source configuration, replication, and access keys.'
              : 'Authenticate to inspect and operate your live MySQL mirrors.'
          }}
        </p>
      </div>
      <form class="stack" @submit.prevent="submitAuth">
        <label>
          <span>Email</span>
          <input v-model="authForm.email" type="email" autocomplete="email" required placeholder="operator@example.com">
        </label>
        <label>
          <span>Password</span>
          <input
            v-model="authForm.password"
            type="password"
            :autocomplete="authMode === 'setup' ? 'new-password' : 'current-password'"
            minlength="12"
            required
            placeholder="At least 12 characters"
          >
        </label>
        <p v-if="error" class="inline-error">{{ error }}</p>
        <button class="button primary wide" :disabled="authenticating">
          <LoaderCircle v-if="authenticating" class="spin" :size="16" />
          {{ authMode === 'setup' ? 'Initialize Pintail' : 'Sign in' }}
          <ArrowRight v-if="!authenticating" :size="16" />
        </button>
      </form>
      <p class="auth-foot">Credentials stay on this Pintail node · Argon2id protected</p>
    </section>
    <aside class="auth-visual" aria-hidden="true">
      <div class="flight-grid">
        <span v-for="index in 28" :key="index" :class="{ signal: [7, 14, 21, 22].includes(index) }" />
      </div>
      <div class="auth-visual-copy">
        <Radio :size="18" />
        <span>Source events become durable analytical blocks.</span>
      </div>
    </aside>
  </main>

  <div v-else class="app-shell">
    <aside class="sidebar">
      <button class="brand" @click="go('overview')">
        <span class="brand-mark">PT</span>
        <span>Pintail</span>
      </button>
      <nav aria-label="Primary navigation">
        <button
          v-for="item in nav"
          :key="item.id"
          :class="{ active: page === item.id || (item.id === 'databases' && page === 'database') }"
          @click="go(item.id)"
        >
          <component :is="item.icon" :size="17" />
          <span>{{ item.label }}</span>
          <span v-if="item.id === 'activity' && alertCount" class="nav-count">{{ alertCount }}</span>
        </button>
      </nav>
      <div class="sidebar-foot">
        <div class="node-status">
          <span class="health-dot" :class="{ stale: error }" />
          <div>
            <strong>{{ error ? 'Attention' : 'Node healthy' }}</strong>
            <span>v0.1.0 · local</span>
          </div>
        </div>
        <button class="icon-button" title="Sign out" @click="logout"><LogOut :size="16" /></button>
      </div>
    </aside>

    <div class="workspace">
      <header class="topbar">
        <div class="breadcrumbs">
          <span>Control plane</span>
          <ChevronRight :size="14" />
          <strong>{{ nav.find((item) => item.id === page)?.label || selectedDatabase?.name }}</strong>
        </div>
        <div class="top-actions">
          <button class="icon-button" :title="dark ? 'Use light theme' : 'Use dark theme'" @click="toggleTheme">
            <Sun v-if="dark" :size="17" />
            <Moon v-else :size="17" />
          </button>
          <button class="button primary compact" @click="beginWizard"><Plus :size="15" /> Add database</button>
        </div>
      </header>

      <div v-if="notice" class="toast" role="status"><Check :size="16" />{{ notice }}</div>
      <div v-if="error" class="alert-strip negative">
        <AlertTriangle :size="17" />
        <span>{{ error }}</span>
        <button @click="error = ''"><X :size="15" /></button>
      </div>

      <section v-if="loading && !databases.length" class="content skeleton-page" aria-label="Loading">
        <div class="skeleton title-skeleton" />
        <div class="metric-grid">
          <div v-for="index in 4" :key="index" class="skeleton metric-skeleton" />
        </div>
        <div class="skeleton table-skeleton" />
      </section>

      <section v-else-if="page === 'overview'" class="content">
        <header class="page-heading split">
          <div>
            <p class="kicker">Live mirror fleet</p>
            <h1>Operations at a glance</h1>
            <p class="muted">Durable source progress, query visibility, and faults on this node.</p>
          </div>
          <button class="button" @click="loadControlPlane"><RefreshCw :size="15" /> Refresh</button>
        </header>

        <div v-if="alertCount" class="alert-strip warning">
          <AlertTriangle :size="17" />
          <span>
            {{ deadLetters.length }} dead-letter event{{ deadLetters.length === 1 ? '' : 's' }};
            {{ databases.filter((item) => item.state === 'needs_resync').length }} mirror{{ databases.filter((item) => item.state === 'needs_resync').length === 1 ? '' : 's' }} need resync.
          </span>
          <button @click="go('activity')">Inspect</button>
        </div>

        <div class="metric-grid">
          <article class="metric-card">
            <span>Databases</span><strong>{{ databases.length }}</strong><small>{{ activeMirrors }} actively converging</small>
          </article>
          <article class="metric-card">
            <span>Rows mirrored</span><strong>{{ formatNumber(totalRows) }}</strong><small>Deduplicated visible rows</small>
          </article>
          <article class="metric-card">
            <span>Recent ingest</span><strong>{{ formatNumber(activity.slice(0, 20).reduce((sum, run) => sum + run.rows, 0)) }}</strong><small>Rows across 20 latest runs</small>
          </article>
          <article class="metric-card signal-card">
            <span>Storage engine</span><strong>v1</strong><small>Checksummed columnar blocks</small>
          </article>
        </div>

        <article class="panel replication-line">
          <div class="panel-heading">
            <div><p class="kicker">Signature path</p><h2>Source → snapshot → stream</h2></div>
            <Badge variant="outline">Durable boundaries only</Badge>
          </div>
          <div class="pipeline">
            <div class="pipeline-node"><Server :size="19" /><strong>Source</strong><span>{{ databases.length }} configured</span></div>
            <div class="pipeline-track"><span :style="{ width: databases.length ? '100%' : '0%' }" /></div>
            <div class="pipeline-node"><HardDrive :size="19" /><strong>Snapshot</strong><span>{{ databases.filter((item) => item.state === 'snapshotting').length }} running</span></div>
            <div class="pipeline-track"><span :style="{ width: activeMirrors ? '100%' : '0%' }" /></div>
            <div class="pipeline-node accent"><Radio :size="19" /><strong>Stream</strong><span>{{ activeMirrors }} live</span></div>
          </div>
        </article>

        <div class="two-column">
          <article class="panel">
            <div class="panel-heading"><h2>Database lag posture</h2><button class="text-button" @click="go('databases')">View all</button></div>
            <div v-if="!databases.length" class="empty-state compact-empty">
              <Database :size="24" /><strong>No source connected</strong><span>Add MySQL to begin the first mirror.</span>
              <button class="button primary" @click="beginWizard">Add database</button>
            </div>
            <div v-else class="database-stack">
              <button v-for="database in databases" :key="database.id" @click="openDatabase(database)">
                <span class="database-glyph">{{ database.name.slice(0, 2).toUpperCase() }}</span>
                <span><strong>{{ database.name }}</strong><small>{{ statuses[database.id]?.rows.toLocaleString() || 0 }} rows</small></span>
                <Badge :class="`tone-${stateTone(database.state)}`">{{ modeOf(database) }}</Badge>
                <ChevronRight :size="15" />
              </button>
            </div>
          </article>
          <article class="panel">
            <div class="panel-heading"><h2>Latest activity</h2><button class="text-button" @click="go('activity')">Open log</button></div>
            <div v-if="!activity.length" class="empty-state compact-empty"><Activity :size="24" /><strong>No sync runs yet</strong><span>Snapshot and replication work appears here.</span></div>
            <ol v-else class="activity-feed">
              <li v-for="record in activity.slice(0, 6)" :key="record.id">
                <span class="event-dot" :class="stateTone(record.status)" />
                <div><strong>{{ record.kind }}</strong><span>{{ databases.find((item) => item.id === record.database_id)?.name || record.database_id }}{{ record.table ? ` · ${record.table}` : '' }}</span></div>
                <time>{{ formatDate(record.started_at) }}</time>
              </li>
            </ol>
          </article>
        </div>
      </section>

      <section v-else-if="page === 'databases'" class="content">
        <header class="page-heading split">
          <div><p class="kicker">Source registry</p><h1>Databases</h1><p class="muted">Every mirror has its own state, checkpoint, and failure boundary.</p></div>
          <button class="button primary" @click="beginWizard"><Plus :size="16" /> Add database</button>
        </header>
        <article class="panel table-panel">
          <div v-if="!databases.length" class="empty-state">
            <Database :size="30" /><h2>No databases yet</h2><p>Connect a source, inspect its capabilities, and choose the tables to mirror.</p>
            <button class="button primary" @click="beginWizard">Start the connection wizard</button>
          </div>
          <div v-else class="table-scroll">
            <table>
              <thead><tr><th>Name</th><th>Mode</th><th>State</th><th>Rows</th><th>Last event</th><th><span class="sr-only">Actions</span></th></tr></thead>
              <tbody>
                <tr v-for="database in databases" :key="database.id">
                  <td><button class="table-link" @click="openDatabase(database)"><span class="database-glyph">{{ database.name.slice(0, 2).toUpperCase() }}</span><strong>{{ database.name }}</strong></button></td>
                  <td><Badge :class="`tone-${modeOf(database) === 'cdc' ? 'positive' : modeOf(database) === 'polling' ? 'warning' : 'neutral'}`">{{ modeOf(database) }}</Badge></td>
                  <td><span class="state-label"><span class="event-dot" :class="stateTone(database.state)" />{{ database.state }}</span></td>
                  <td class="mono">{{ statuses[database.id]?.rows.toLocaleString() || 0 }}</td>
                  <td class="muted">{{ formatDate(database.updated_at) }}</td>
                  <td>
                    <div class="row-actions">
                      <button class="icon-button" :title="database.mode === 'paused' ? 'Resume' : 'Pause'" @click="setMode(database, database.mode === 'paused' ? 'auto' : 'paused')">
                        <Play v-if="database.mode === 'paused'" :size="15" /><Pause v-else :size="15" />
                      </button>
                      <button class="icon-button danger" title="Delete" @click="deleteCandidate = database; deleteText = ''"><Trash2 :size="15" /></button>
                    </div>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
        </article>
      </section>

      <section v-else-if="page === 'database' && selectedDatabase" class="content">
        <header class="page-heading split database-heading">
          <div>
            <button class="back-link" @click="go('databases')">Databases /</button>
            <h1>{{ selectedDatabase.name }}</h1>
            <div class="heading-badges">
              <Badge :class="`tone-${stateTone(selectedDatabase.state)}`">{{ selectedDatabase.state }}</Badge>
              <Badge variant="outline">{{ modeOf(selectedDatabase) }}</Badge>
              <span>{{ statuses[selectedDatabase.id]?.rows.toLocaleString() || 0 }} visible rows</span>
            </div>
          </div>
          <div class="top-actions">
            <button class="button" @click="setMode(selectedDatabase, selectedDatabase.mode === 'paused' ? 'auto' : 'paused')">
              <Play v-if="selectedDatabase.mode === 'paused'" :size="15" /><Pause v-else :size="15" />
              {{ selectedDatabase.mode === 'paused' ? 'Resume' : 'Pause' }}
            </button>
            <button class="button primary" @click="forceSnapshot"><RefreshCw :size="15" /> Resnapshot</button>
          </div>
        </header>

        <div v-if="modeOf(selectedDatabase) === 'polling'" class="alert-strip warning">
          <AlertTriangle :size="17" />
          <span>Polling mode converges deletes during reconciliation and can miss intermediate states between polls.</span>
        </div>

        <nav class="tabs" aria-label="Database detail">
          <button v-for="tab in ['tables', 'snapshot', 'replication', 'schema', 'storage', 'settings']" :key="tab" :class="{ active: detailTab === tab }" @click="detailTab = tab">{{ tab }}</button>
        </nav>

        <article v-if="detailTab === 'tables'" class="panel table-panel">
          <div v-if="!tables.length" class="empty-state compact-empty"><Table2 :size="26" /><strong>No mirrored tables</strong><span>Run a snapshot or revise the include list.</span></div>
          <div v-else class="table-scroll">
            <table><thead><tr><th>Table</th><th>State</th><th>Rows</th><th>Schema</th><th>Fault</th><th>Action</th></tr></thead>
              <tbody><tr v-for="table in tables" :key="table.name">
                <td><strong>{{ table.name }}</strong></td><td><Badge :class="`tone-${stateTone(table.state)}`">{{ table.state }}</Badge></td>
                <td class="mono">{{ table.rows.toLocaleString() }}</td><td class="mono">v{{ table.schema_version }}</td><td class="muted">{{ table.last_error || '—' }}</td>
                <td><div class="row-actions">
                  <button class="text-button" :disabled="Boolean(tableAction)" @click="runTableAction(table, 'reconcile')">
                    <LoaderCircle v-if="tableAction === `${table.name}:reconcile`" class="spin" :size="13" /> Reconcile
                  </button>
                  <button class="text-button" :disabled="Boolean(tableAction)" title="Starts a mirror-wide resnapshot because all tables share one source checkpoint" @click="runTableAction(table, 'resync')">
                    <LoaderCircle v-if="tableAction === `${table.name}:resync`" class="spin" :size="13" /> Resync
                  </button>
                </div></td>
              </tr></tbody>
            </table>
          </div>
        </article>

        <div v-else-if="detailTab === 'snapshot'" class="stack">
          <article class="panel progress-overview">
            <div><p class="kicker">Durable publication</p><h2>{{ snapshot?.state || selectedDatabase.state }}</h2><p class="muted">Progress advances only after a chunk and its control-plane checkpoint are durable.</p></div>
            <div class="progress-stat"><strong>{{ snapshot?.tables.reduce((sum, table) => sum + table.rows, 0).toLocaleString() || 0 }}</strong><span>rows published</span></div>
          </article>
          <article class="panel">
            <div v-if="!snapshot?.tables.length" class="empty-state compact-empty"><HardDrive :size="26" /><strong>No snapshot journal</strong><span>Start a snapshot to see per-table progress.</span></div>
            <div v-else class="progress-list">
              <div v-for="table in snapshot.tables" :key="table.name" class="progress-row">
                <div><strong>{{ table.name }}</strong><span>{{ table.completed_chunks }}/{{ table.total_chunks }} chunks · {{ table.rows.toLocaleString() }} rows</span></div>
                <div class="progress-track"><span :style="{ width: `${snapshotPercent(table)}%` }" /></div>
                <strong class="mono">{{ snapshotPercent(table) }}%</strong>
              </div>
            </div>
          </article>
        </div>

        <div v-else-if="detailTab === 'replication'" class="two-column">
          <article class="panel">
            <div class="panel-heading"><div><p class="kicker">Checkpoint</p><h2>{{ modeOf(selectedDatabase) }}</h2></div><Radio :size="20" /></div>
            <dl class="definition-grid"><div><dt>State</dt><dd>{{ selectedDatabase.state }}</dd></div><div><dt>Poll cadence</dt><dd>{{ selectedDatabase.poll_interval_seconds }}s</dd></div><div><dt>Reconcile</dt><dd>{{ selectedDatabase.reconcile_interval_seconds }}s</dd></div><div><dt>Updated</dt><dd>{{ formatDate(selectedDatabase.updated_at) }}</dd></div></dl>
          </article>
          <article class="panel">
            <div class="panel-heading"><h2>Dead-letter queue</h2><Badge :class="deadLetters.filter((item) => item.database_id === selectedDatabase?.id).length ? 'tone-negative' : 'tone-positive'">{{ deadLetters.filter((item) => item.database_id === selectedDatabase?.id).length }}</Badge></div>
            <div v-if="!deadLetters.filter((item) => item.database_id === selectedDatabase?.id).length" class="empty-state compact-empty"><Check :size="24" /><strong>No rejected events</strong><span>Decoder and storage errors appear here.</span></div>
            <div v-for="record in deadLetters.filter((item) => item.database_id === selectedDatabase?.id)" :key="record.id" class="dlq-card"><strong>{{ record.table || 'database' }}</strong><p>{{ record.error }}</p><button class="text-button" @click="discardDlq(record)">Discard</button></div>
          </article>
        </div>

        <article v-else-if="detailTab === 'schema'" class="panel">
          <div class="panel-heading"><div><p class="kicker">Replica catalog</p><h2>Schema generations</h2></div><Badge variant="outline">{{ tables.length }} tables</Badge></div>
          <div class="schema-grid"><button v-for="table in tables" :key="table.name" @click="describeTable(table)"><Table2 :size="16" /><span><strong>{{ table.name }}</strong><small>Generation {{ table.schema_version }}</small></span><ChevronRight :size="15" /></button></div>
        </article>

        <article v-else-if="detailTab === 'storage'" class="panel">
          <div class="panel-heading"><div><p class="kicker">Columnar footprint</p><h2>Storage posture</h2></div><HardDrive :size="20" /></div>
          <div class="metric-grid three"><div class="metric-card"><span>Visible rows</span><strong>{{ formatNumber(statuses[selectedDatabase.id]?.rows || 0) }}</strong><small>Merge-on-read deduplicated</small></div><div class="metric-card"><span>Schema generations</span><strong>{{ tables.reduce((sum, table) => sum + table.schema_version, 0) }}</strong><small>Stable column IDs</small></div><div class="metric-card"><span>Compaction</span><strong>Auto</strong><small>Bounded size-tier passes</small></div></div>
          <p class="muted panel-note">Exact segment bytes and compression ratios are exported by the operations metrics surface in M8.</p>
        </article>

        <article v-else class="panel settings-form">
          <div class="panel-heading"><div><p class="kicker">Replication controls</p><h2>Database settings</h2></div></div>
          <label><span>Requested mode</span><select :value="selectedDatabase.mode" @change="setMode(selectedDatabase, ($event.target as HTMLSelectElement).value as DatabaseRecord['mode'])"><option value="auto">Auto</option><option value="cdc">CDC</option><option value="polling">Polling</option><option value="paused">Paused</option></select></label>
          <div class="definition-grid"><div><dt>Poll cadence</dt><dd>{{ selectedDatabase.poll_interval_seconds }} seconds</dd></div><div><dt>Reconciliation</dt><dd>{{ selectedDatabase.reconcile_interval_seconds }} seconds</dd></div><div><dt>Included</dt><dd>{{ selectedDatabase.include_tables.length || 'All tables' }}</dd></div><div><dt>Excluded</dt><dd>{{ selectedDatabase.exclude_tables.length || 'None' }}</dd></div></div>
        </article>
      </section>

      <section v-else-if="page === 'wizard'" class="content wizard-page">
        <header class="page-heading"><p class="kicker">Add database</p><h1>Build a live mirror</h1><p class="muted">Connection, capability proof, table selection, then durable handoff.</p></header>
        <ol class="stepper">
          <li v-for="(label, index) in ['Connection', 'Probe', 'Tables', 'Start']" :key="label" :class="{ active: wizard.step === index + 1, complete: wizard.step > index + 1 }"><span>{{ wizard.step > index + 1 ? '✓' : index + 1 }}</span>{{ label }}</li>
        </ol>
        <article class="panel wizard-panel">
          <form v-if="wizard.step === 1" class="wizard-form" @submit.prevent="wizardConnection">
            <div><p class="kicker">01 / Connection</p><h2>Where is MySQL?</h2><p class="muted">The DSN is encrypted before it enters the control-plane database.</p></div>
            <div class="form-grid"><label><span>MySQL schema</span><input v-model="wizard.name" required placeholder="analytics"><small>Exact source schema name and case.</small></label><label class="full"><span>MySQL DSN</span><input v-model="wizard.dsn" required type="password" placeholder="mysql://pintail:secret@db.internal/analytics"></label></div>
            <p v-if="wizard.error" class="inline-error">{{ wizard.error }}</p>
            <div class="wizard-actions"><button type="button" class="button" @click="go('databases')">Cancel</button><button class="button primary" :disabled="wizard.working"><LoaderCircle v-if="wizard.working" class="spin" :size="15" /> Test connection <ArrowRight v-if="!wizard.working" :size="15" /></button></div>
          </form>
          <div v-else-if="wizard.step === 2" class="wizard-form">
            <div><p class="kicker">02 / Capability probe</p><h2>{{ wizard.serverVersion }}</h2><p class="muted">Pintail checks every invariant required for safe snapshot and stream ownership.</p></div>
            <div v-if="wizard.probe" class="checklist">
              <div v-for="(value, key) in wizard.probe.capabilities" v-show="typeof value === 'boolean'" :key="key"><span :class="value ? 'check-positive' : 'check-negative'"><Check v-if="value" :size="14" /><X v-else :size="14" /></span><strong>{{ String(key).replaceAll('_', ' ') }}</strong><small>{{ value ? 'Pass' : 'Requires remediation' }}</small></div>
            </div>
            <div class="recommendation"><Radio :size="18" /><div><strong>Recommended: {{ wizard.probe?.capabilities.recommended_mode.toUpperCase() }}</strong><span>{{ wizard.probe?.capabilities.reasons.join(' · ') || 'All native replication requirements passed.' }}</span></div></div>
            <fieldset class="mode-choice"><legend>Replication mode</legend><label><input v-model="wizard.mode" type="radio" value="cdc"> CDC</label><label><input v-model="wizard.mode" type="radio" value="polling"> Polling</label></fieldset>
            <div class="wizard-actions"><button class="button" @click="wizard.step = 1">Back</button><button class="button primary" @click="wizard.step = 3">Choose tables <ArrowRight :size="15" /></button></div>
          </div>
          <div v-else-if="wizard.step === 3 && wizard.probe" class="wizard-form">
            <div><p class="kicker">03 / Table selection</p><h2>Choose the analytical surface</h2><p class="muted">PK-less append tables preserve rows but cannot model source updates or deletes.</p></div>
            <div class="table-picker">
              <label v-for="table in wizard.probe.tables" :key="table.name"><input v-model="wizard.includes" type="checkbox" :value="table.name"><span><strong>{{ table.name }}</strong><small>{{ table.estimated_rows?.toLocaleString() || 'Unknown' }} rows · {{ table.engine || 'Unknown engine' }}</small></span><Badge :class="table.key.mode === 'append_row_id' ? 'tone-warning' : 'tone-positive'">{{ table.key.mode.replace('_', ' ') }}</Badge><AlertTriangle v-if="table.warnings.length" :size="16" /></label>
            </div>
            <p v-if="wizard.error" class="inline-error">{{ wizard.error }}</p>
            <div class="wizard-actions"><button class="button" @click="wizard.step = 2">Back</button><button class="button primary" :disabled="wizard.working || !wizard.includes.length" @click="finishWizard"><LoaderCircle v-if="wizard.working" class="spin" :size="15" /> Review & start <ArrowRight v-if="!wizard.working" :size="15" /></button></div>
          </div>
          <div v-else class="empty-state"><LoaderCircle class="spin" :size="28" /><h2>Starting the mirror</h2><p>Capturing the source position and establishing resumable chunks.</p></div>
        </article>
      </section>

      <section v-else-if="page === 'sql'" class="content sql-page">
        <header class="page-heading split"><div><p class="kicker">Native query engine</p><h1>SQL Console</h1><p class="muted">MySQL dialect over reader-pinned columnar snapshots.</p></div><div class="select-wrap"><Database :size="15" /><select v-model="sqlDatabaseId"><option disabled value="">Choose database</option><option v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</option></select></div></header>
        <div v-if="!databases.length" class="panel empty-state"><SquareTerminal :size="30" /><h2>No queryable mirror</h2><p>Add and snapshot a database before opening the console.</p><button class="button primary" @click="beginWizard">Add database</button></div>
        <template v-else>
          <article class="panel editor-panel">
            <div class="editor-toolbar"><span>query.sql</span><div><span class="shortcut">⌘ Enter</span><button class="button primary compact" :disabled="sqlRunning" @click="runSql"><LoaderCircle v-if="sqlRunning" class="spin" :size="14" /><Play v-else :size="14" /> Run</button></div></div>
            <LazySqlEditor v-model="sqlText" @run="runSql" />
          </article>
          <p v-if="sqlError" class="inline-error sql-error">{{ sqlError }}</p>
          <article class="panel result-panel">
            <div class="panel-heading"><div><h2>Results</h2><p v-if="sqlResult" class="muted">{{ sqlResult.stats.rows }} rows · {{ sqlResult.stats.duration_ms }} ms · {{ sqlResult.stats.blocks_read }} blocks read / {{ sqlResult.stats.blocks_pruned }} pruned</p></div><div v-if="sqlResult" class="row-actions"><button class="button compact" @click="exportResult('csv')">CSV</button><button class="button compact" @click="exportResult('json')">JSON</button></div></div>
            <div v-if="!sqlResult" class="empty-state compact-empty"><Search :size="24" /><strong>Run a query</strong><span>Typed fields and physical scan counters appear here.</span></div>
            <div v-else class="table-scroll result-scroll"><table><thead><tr><th v-for="field in sqlResult.fields" :key="field.name"><span>{{ field.name }}</span><small>{{ typeof field.data_type === 'string' ? field.data_type : 'typed' }}</small></th></tr></thead><tbody><tr v-for="(row, rowIndex) in sqlResult.rows" :key="rowIndex"><td v-for="(value, valueIndex) in row" :key="valueIndex" :class="{ null: value === null }">{{ displayValue(value) }}</td></tr></tbody></table></div>
          </article>
        </template>
      </section>

      <section v-else-if="page === 'activity'" class="content">
        <header class="page-heading split"><div><p class="kicker">Durable work log</p><h1>Activity</h1><p class="muted">Snapshot, stream, poll, and repair outcomes from control-plane records.</p></div><div class="select-wrap"><Database :size="15" /><select v-model="activityDatabase"><option value="">All databases</option><option v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</option></select></div></header>
        <article class="panel table-panel">
          <div v-if="!filteredActivity.length" class="empty-state"><Activity :size="28" /><h2>No matching activity</h2><p>Completed and failed replication work appears after the first snapshot.</p></div>
          <div v-else class="table-scroll"><table><thead><tr><th>Started</th><th>Database</th><th>Kind</th><th>Status</th><th>Rows</th><th>Bytes</th><th>Duration</th></tr></thead><tbody><tr v-for="record in filteredActivity" :key="record.id"><td class="muted">{{ formatDate(record.started_at) }}</td><td><strong>{{ databases.find((item) => item.id === record.database_id)?.name || record.database_id }}</strong><small v-if="record.table">{{ record.table }}</small></td><td>{{ record.kind }}</td><td><Badge :class="`tone-${stateTone(record.status)}`">{{ record.status }}</Badge></td><td class="mono">{{ record.rows.toLocaleString() }}</td><td class="mono">{{ formatBytes(record.bytes) }}</td><td class="mono">{{ record.duration_ms === null ? '—' : `${record.duration_ms} ms` }}</td></tr></tbody></table></div>
        </article>
        <article v-if="deadLetters.length" class="panel dlq-panel"><div class="panel-heading"><div><p class="kicker">Requires judgment</p><h2>Dead-letter queue</h2></div><Badge class="tone-negative">{{ deadLetters.length }}</Badge></div><div class="dlq-grid"><div v-for="record in deadLetters" :key="record.id" class="dlq-card"><div><strong>{{ record.table || 'Database event' }}</strong><span>{{ formatDate(record.created_at) }}</span></div><p>{{ record.error }}</p><pre>{{ JSON.stringify(record.event, null, 2) }}</pre><button class="button compact" @click="discardDlq(record)">Discard</button></div></div></article>
      </section>

      <section v-else-if="page === 'keys'" class="content">
        <header class="page-heading split"><div><p class="kicker">Database-scoped access</p><h1>API Keys</h1><p class="muted">Secrets are SHA-256 hash-only and shown once.</p></div><div class="select-wrap"><Database :size="15" /><select v-model="keyDatabaseId" @change="loadKeys"><option disabled value="">Choose database</option><option v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</option></select></div></header>
        <article class="panel key-create"><div><h2>Create a key</h2><p class="muted">Use a narrow scope for each application.</p></div><label><span>Name</span><input v-model="keyForm.name" placeholder="Metabase production"></label><fieldset><legend>Scopes</legend><label><input v-model="keyForm.scopes" type="checkbox" value="read"> Read metadata</label><label><input v-model="keyForm.scopes" type="checkbox" value="query"> Run queries</label></fieldset><button class="button primary" :disabled="!keyDatabaseId || !keyForm.name || !keyForm.scopes.length" @click="createKey"><Plus :size="15" /> Create</button></article>
        <div v-if="revealedSecret" class="secret-banner"><AlertTriangle :size="18" /><div><strong>Copy this secret now. It cannot be recovered.</strong><code>{{ revealedSecret }}</code></div><button class="icon-button" @click="copy(revealedSecret)"><Copy :size="16" /></button><button class="icon-button" @click="revealedSecret = ''"><X :size="16" /></button></div>
        <article class="panel table-panel"><div v-if="!keys.length" class="empty-state"><KeyRound :size="28" /><h2>No keys for this database</h2><p>Create one for the HTTP API or MySQL wire clients.</p></div><div v-else class="table-scroll"><table><thead><tr><th>Name</th><th>Scopes</th><th>Status</th><th>Last used</th><th>Created</th><th></th></tr></thead><tbody><tr v-for="key in keys" :key="key.id"><td><strong>{{ key.name }}</strong><small>{{ key.id }}</small></td><td><Badge v-for="scope in key.scopes" :key="scope" variant="outline">{{ scope }}</Badge></td><td><Badge :class="key.enabled ? 'tone-positive' : 'tone-neutral'">{{ key.enabled ? 'enabled' : 'disabled' }}</Badge></td><td class="muted">{{ formatDate(key.last_used_at) }}</td><td class="muted">{{ formatDate(key.created_at) }}</td><td><div class="row-actions"><button class="text-button" @click="toggleKey(key)">{{ key.enabled ? 'Disable' : 'Enable' }}</button><button class="icon-button danger" @click="deleteKey(key)"><Trash2 :size="14" /></button></div></td></tr></tbody></table></div></article>
      </section>

      <section v-else-if="page === 'backups'" class="content">
        <header class="page-heading"><p class="kicker">Recovery plane</p><h1>Backups</h1><p class="muted">Full and incremental artifacts will preserve manifests, segments, and metadata as one unit.</p></header>
        <div class="two-column">
          <article class="panel settings-form"><div class="panel-heading"><div><p class="kicker">S3 destination</p><h2>Backup configuration</h2></div><Badge class="tone-warning">M8 activation</Badge></div><label><span>Database</span><select v-model="keyDatabaseId"><option v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</option></select></label><label><span>Bucket URL</span><input disabled placeholder="s3://analytics-backups/pintail"></label><label><span>Schedule</span><select disabled><option>Daily at 02:00</option></select></label><button class="button primary" disabled>Save configuration</button><p class="muted panel-note">The page is deliberately read-only until the M8 backup/restore gate proves round-trip recovery.</p></article>
          <article class="panel"><div class="panel-heading"><h2>Backup history</h2><Archive :size="19" /></div><div class="empty-state compact-empty"><Archive :size="26" /><strong>No backup artifacts</strong><span>Backup execution and restore-as arrive with the verified M8 operations engine.</span></div></article>
        </div>
      </section>

      <section v-else-if="page === 'settings'" class="content">
        <header class="page-heading"><p class="kicker">Node policy</p><h1>Settings</h1><p class="muted">Operator identity, network surfaces, and local presentation.</p></header>
        <div class="settings-grid">
          <article class="panel"><div class="panel-heading"><div><p class="kicker">Operator</p><h2>Current session</h2></div><Server :size="19" /></div><dl class="definition-grid"><div><dt>Subject</dt><dd class="mono">{{ session.subject }}</dd></div><div><dt>Role</dt><dd>{{ session.role }}</dd></div><div><dt>Scopes</dt><dd>{{ session.scopes.join(', ') }}</dd></div><div><dt>Session</dt><dd>12-hour signed JWT</dd></div></dl></article>
          <article class="panel"><div class="panel-heading"><div><p class="kicker">Appearance</p><h2>Interface</h2></div><button class="icon-button" @click="toggleTheme"><Sun v-if="dark" :size="16" /><Moon v-else :size="16" /></button></div><button class="setting-row" @click="toggleTheme"><span><strong>Dark instrument panel</strong><small>Stored only in this browser.</small></span><span class="switch" :class="{ on: dark }"><span /></span></button></article>
          <article class="panel wire-status"><div class="panel-heading"><div><p class="kicker">MySQL wire</p><h2>Client endpoint</h2></div><Badge :class="nodeStatus?.wire.enabled ? 'tone-positive' : 'tone-negative'">{{ nodeStatus?.wire.enabled ? 'Live' : 'Unavailable' }}</Badge></div><div class="endpoint-line"><span class="endpoint-pulse" :class="{ live: nodeStatus?.wire.enabled }" /><code>{{ nodeStatus?.wire.bind || 'Endpoint unavailable' }}</code></div><dl class="definition-grid"><div><dt>Mode</dt><dd>Read-only</dd></div><div><dt>Authentication</dt><dd>Database API key</dd></div><div><dt>Username</dt><dd>Database name</dd></div><div><dt>Protocol</dt><dd>MySQL native</dd></div></dl></article>
          <article class="panel"><div class="panel-heading"><div><p class="kicker">Telemetry</p><h2>Operations</h2></div><Badge class="tone-warning">M8</Badge></div><dl class="definition-grid"><div><dt>Metrics</dt><dd>/metrics</dd></div><div><dt>Exposure</dt><dd>Loopback</dd></div><div><dt>Log level</dt><dd>info</dd></div><div><dt>Supervisor</dt><dd>Finite handoff</dd></div></dl></article>
        </div>
      </section>

      <section v-else-if="page === 'connect'" class="content">
        <header class="page-heading"><p class="kicker">Client handoff</p><h1>Connect to Pintail</h1><p class="muted">The database name is the username; its scoped API key is the password.</p></header>
        <form class="panel connect-controls" @submit.prevent><label><span>Database</span><select v-model="keyDatabaseId"><option v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</option></select></label><label><span>Client host</span><input v-model="connectHost" autocomplete="url"></label><label><span>Wire port</span><input v-model="connectPort" inputmode="numeric"></label><label><span>Query-scoped API key</span><input v-model="connectKey" type="password" autocomplete="off"></label></form>
        <div class="protocol-note"><Radio :size="17" /><div><strong>Native challenge, no stored plaintext.</strong><span>Use MySQL 8.4, mysql2, PyMySQL, DBeaver, or Metabase. Oracle's MySQL 9.x CLI removed its native-password client plugin.</span></div><button class="text-button" @click="go('keys')">Create or rotate key <ArrowRight :size="13" /></button></div>
        <div class="snippet-grid">
          <article v-for="kind in (['mysql', 'node', 'python'] as const)" :key="kind" class="panel snippet"><div class="panel-heading"><h2>{{ kind === 'mysql' ? 'MySQL CLI' : kind === 'node' ? 'Node.js' : 'Python' }}</h2><button class="icon-button" @click="copy(connectSnippet(kind))"><Copy :size="15" /></button></div><pre>{{ connectSnippet(kind) }}</pre></article>
          <article class="panel snippet"><div class="panel-heading"><h2>DBeaver / Metabase</h2><CircleHelp :size="17" /></div><dl class="definition-grid"><div><dt>Driver</dt><dd>MySQL 8</dd></div><div><dt>Host / port</dt><dd>{{ connectHost }}:{{ connectPort }}</dd></div><div><dt>Database / user</dt><dd>{{ selectedConnectDatabase?.name || 'analytics' }}</dd></div><div><dt>Password</dt><dd>Query-scoped API key</dd></div></dl><p class="muted panel-note">Keep SSL disabled for a loopback endpoint. Terminate TLS at your private ingress when clients connect across a network.</p></article>
        </div>
      </section>
    </div>

    <div v-if="deleteCandidate" class="modal-backdrop" @click.self="deleteCandidate = null">
      <section class="modal" role="dialog" aria-modal="true" aria-labelledby="delete-title">
        <div class="modal-icon"><Trash2 :size="20" /></div><h2 id="delete-title">Remove {{ deleteCandidate.name }}?</h2><p>The source configuration is deleted. Mirrored storage is retained for manual recovery.</p><label><span>Type <strong>{{ deleteCandidate.name }}</strong> to confirm</span><input v-model="deleteText" autofocus></label><div class="wizard-actions"><button class="button" @click="deleteCandidate = null">Cancel</button><button class="button danger-button" :disabled="deleteText !== deleteCandidate.name" @click="removeDatabase">Remove database</button></div>
      </section>
    </div>
  </div>
</template>
