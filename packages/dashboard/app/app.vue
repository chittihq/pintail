<script setup lang="ts">
import {
  Activity,
  AlertTriangle,
  Archive,
  ArrowRight,
  Cable,
  Check,
  ChevronRight,
  ChevronsUpDown,
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
import { toast } from 'vue-sonner'
import 'vue-sonner/style.css'
import type {
  ActivityRecord,
  ApiKeyRecord,
  BackupConfig,
  BackupRecord,
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
const backupDatabaseId = ref('')
const backups = ref<BackupRecord[]>([])
const backupLoading = ref(false)
const backupConfigLoaded = ref(false)
const backupForm = reactive({
  bucket: '',
  prefix: 'pintail',
  endpoint: '',
  region: 'us-east-1',
  accessKeyId: '',
  secretAccessKey: '',
  scheduleMinutes: 1_440,
  enabled: true,
})
const restoreBackupId = ref('')
const restoreName = ref('')

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
    if (!validIds.has(backupDatabaseId.value)) backupDatabaseId.value = fallbackId
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
    if (page.value === 'backups' && backupDatabaseId.value) {
      await loadBackups(false)
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
  if (target === 'backups') void loadBackups()
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

async function retryDlq(record: DlqRecord) {
  try {
    await request(`/dlq/${record.id}/retry`, { method: 'POST' })
    toast(`${record.table || 'Database'} recovered; dead letter cleared`)
    await refreshLiveData()
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

async function loadBackups(showLoading = true) {
  if (!backupDatabaseId.value) {
    backups.value = []
    backupConfigLoaded.value = false
    return
  }
  if (showLoading) backupLoading.value = true
  try {
    backups.value = await request<BackupRecord[]>(
      `/databases/${backupDatabaseId.value}/backups`,
    )
    const completed = backups.value.find((backup) => backup.status === 'completed')
    if (!restoreBackupId.value || !backups.value.some((backup) => backup.id === restoreBackupId.value)) {
      restoreBackupId.value = completed?.id || ''
    }
    const config = await request<BackupConfig>(
      `/databases/${backupDatabaseId.value}/backup-config`,
    )
    backupForm.bucket = config.bucket
    backupForm.prefix = config.prefix
    backupForm.endpoint = config.endpoint || ''
    backupForm.region = config.region
    backupForm.scheduleMinutes = config.schedule_minutes
    backupForm.enabled = config.enabled
    backupForm.accessKeyId = ''
    backupForm.secretAccessKey = ''
    backupConfigLoaded.value = config.configured
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    if (showLoading) backupLoading.value = false
  }
}

async function saveBackupConfig() {
  if (!backupDatabaseId.value || !backupForm.bucket.trim() || !backupForm.prefix.trim()) return
  backupLoading.value = true
  try {
    await request<BackupConfig>(
      `/databases/${backupDatabaseId.value}/backup-config`,
      {
        method: 'PUT',
        body: JSON.stringify({
          bucket: backupForm.bucket.trim(),
          prefix: backupForm.prefix.trim(),
          endpoint: backupForm.endpoint.trim() || null,
          region: backupForm.region.trim() || 'us-east-1',
          access_key_id: backupForm.accessKeyId.trim() || null,
          secret_access_key: backupForm.secretAccessKey || null,
          schedule_minutes: Math.max(1, backupForm.scheduleMinutes),
          enabled: backupForm.enabled,
        }),
      },
    )
    toast('Backup destination saved')
    await loadBackups(false)
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    backupLoading.value = false
  }
}

async function runBackup(full: boolean) {
  if (!backupDatabaseId.value) return
  backupLoading.value = true
  try {
    await request(`/databases/${backupDatabaseId.value}/backups`, {
      method: 'POST',
      body: JSON.stringify({ full }),
    })
    toast(full ? 'Full backup started' : 'Backup started')
    await loadBackups(false)
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    backupLoading.value = false
  }
}

async function restoreSelectedBackup() {
  if (!backupDatabaseId.value || !restoreBackupId.value || !restoreName.value.trim()) return
  backupLoading.value = true
  try {
    await request(`/databases/${backupDatabaseId.value}/backups/restore`, {
      method: 'POST',
      body: JSON.stringify({
        backup_id: restoreBackupId.value,
        name: restoreName.value.trim(),
      }),
    })
    restoreName.value = ''
    toast('Backup restored as a new detached database')
    await loadControlPlane()
    await loadBackups(false)
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    backupLoading.value = false
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

function toggleScope(scope: string, on: boolean) {
  keyForm.scopes = on
    ? [...new Set([...keyForm.scopes, scope])]
    : keyForm.scopes.filter((existing) => existing !== scope)
}

function toggleInclude(name: string, on: boolean) {
  wizard.includes = on
    ? [...new Set([...wizard.includes, name])]
    : wizard.includes.filter((existing) => existing !== name)
}

function closeDeleteDialog(open: boolean) {
  if (!open) {
    deleteCandidate.value = null
    deleteText.value = ''
  }
}

function initials(subject: string) {
  return subject.split('@')[0]!.slice(0, 2).toUpperCase()
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
  <Toaster position="top-right" />

  <div v-if="booting" class="boot-screen" aria-live="polite">
    <div class="brand-mark">PT</div>
    <LoaderCircle class="spin" :size="20" />
    <span>Opening control plane</span>
  </div>

  <main v-else-if="!session" class="bg-muted flex min-h-svh items-center justify-center p-6 md:p-10">
    <div class="w-full max-w-4xl">
      <Card class="overflow-hidden p-0">
        <CardContent class="grid p-0 md:grid-cols-2">
          <form class="p-6 md:p-8" @submit.prevent="submitAuth">
            <div class="flex flex-col gap-6">
              <div class="flex flex-col items-center gap-2 text-center">
                <span class="brand-mark">PT</span>
                <h1 class="text-xl font-bold">{{ authMode === 'setup' ? 'Create the operator' : 'Welcome back' }}</h1>
                <p class="text-muted-foreground text-balance">
                  {{
                    authMode === 'setup'
                      ? 'This one-time account owns source configuration, replication, and access keys.'
                      : 'Authenticate to inspect and operate your live MySQL mirrors.'
                  }}
                </p>
              </div>
              <div class="grid gap-1.5">
                <Label for="auth-email">Email</Label>
                <Input id="auth-email" v-model="authForm.email" type="email" autocomplete="email" required placeholder="operator@example.com" />
              </div>
              <div class="grid gap-1.5">
                <Label for="auth-password">Password</Label>
                <Input
                  id="auth-password"
                  v-model="authForm.password"
                  type="password"
                  :autocomplete="authMode === 'setup' ? 'new-password' : 'current-password'"
                  minlength="12"
                  required
                  placeholder="At least 12 characters"
                />
              </div>
              <p v-if="error" class="inline-error">{{ error }}</p>
              <Button type="submit" class="w-full" :disabled="authenticating">
                <LoaderCircle v-if="authenticating" class="spin" />
                {{ authMode === 'setup' ? 'Initialize Pintail' : 'Sign in' }}
                <ArrowRight v-if="!authenticating" />
              </Button>
              <p class="text-muted-foreground text-center text-xs">Credentials stay on this Pintail node · Argon2id protected</p>
            </div>
          </form>
          <aside class="auth-visual" aria-hidden="true">
            <div class="flight-grid">
              <span v-for="index in 28" :key="index" :class="{ signal: [7, 14, 21, 22].includes(index) }" />
            </div>
            <div class="auth-visual-copy">
              <Radio :size="18" />
              <span>Source events become durable analytical blocks.</span>
            </div>
          </aside>
        </CardContent>
      </Card>
    </div>
  </main>

  <SidebarProvider
    v-else
    :style="{ '--sidebar-width': 'calc(var(--spacing) * 64)', '--header-height': 'calc(var(--spacing) * 14)' }"
  >
    <Sidebar variant="inset" collapsible="icon">
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <SidebarMenuButton size="lg" class="data-[slot=sidebar-menu-button]:!p-1.5 font-heading text-base font-extrabold tracking-tight" @click="go('overview')">
              <span class="brand-mark shrink-0">PT</span>
              <span>Pintail</span>
            </SidebarMenuButton>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>
      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              <SidebarMenuItem v-for="item in nav" :key="item.id">
                <SidebarMenuButton
                  :is-active="page === item.id || (item.id === 'databases' && page === 'database')"
                  :tooltip="item.label"
                  @click="go(item.id)"
                >
                  <component :is="item.icon" />
                  <span>{{ item.label }}</span>
                </SidebarMenuButton>
                <SidebarMenuBadge
                  v-if="item.id === 'activity' && alertCount"
                  class="bg-red rounded-full font-mono text-[0.6rem] text-white"
                >
                  {{ alertCount }}
                </SidebarMenuBadge>
              </SidebarMenuItem>
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
      <SidebarFooter>
        <SidebarMenu>
          <SidebarMenuItem>
            <DropdownMenu>
              <DropdownMenuTrigger as-child>
                <SidebarMenuButton size="lg" class="data-[state=open]:bg-sidebar-accent data-[state=open]:text-sidebar-accent-foreground">
                  <Avatar class="size-8 rounded-md">
                    <AvatarFallback class="rounded-md">{{ initials(session.subject) }}</AvatarFallback>
                  </Avatar>
                  <div class="grid flex-1 text-left text-sm leading-tight">
                    <span class="flex items-center gap-1.5 truncate font-medium">
                      <span class="health-dot shrink-0" :class="{ stale: error }" />
                      {{ error ? 'Attention' : 'Node healthy' }}
                    </span>
                    <span class="text-sidebar-foreground/60 truncate text-xs">{{ session.subject }}</span>
                  </div>
                  <ChevronsUpDown class="ml-auto size-4" />
                </SidebarMenuButton>
              </DropdownMenuTrigger>
              <DropdownMenuContent class="w-(--reka-dropdown-menu-trigger-width) min-w-56 rounded-lg" side="top" align="start" :side-offset="4">
                <DropdownMenuLabel class="p-0 font-normal">
                  <div class="flex items-center gap-2 px-1 py-1.5 text-left text-sm">
                    <Avatar class="size-8 rounded-md">
                      <AvatarFallback class="rounded-md">{{ initials(session.subject) }}</AvatarFallback>
                    </Avatar>
                    <div class="grid flex-1 text-left text-sm leading-tight">
                      <span class="truncate font-medium">{{ session.subject }}</span>
                      <span class="text-muted-foreground truncate text-xs">{{ session.role }} · v0.1.0</span>
                    </div>
                  </div>
                </DropdownMenuLabel>
                <DropdownMenuSeparator />
                <DropdownMenuItem @click="go('settings')">
                  <Settings /> Settings
                </DropdownMenuItem>
                <DropdownMenuSeparator />
                <DropdownMenuItem @click="logout">
                  <LogOut /> Sign out
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>

    <SidebarInset class="min-w-0">
      <header class="flex h-(--header-height) shrink-0 items-center gap-2 border-b transition-[width,height] ease-linear">
        <div class="flex w-full items-center gap-1 px-4 lg:gap-2 lg:px-6">
          <SidebarTrigger class="-ml-1" />
          <Separator orientation="vertical" class="mx-2 data-[orientation=vertical]:h-4" />
          <div class="breadcrumbs">
            <span>Control plane</span>
            <ChevronRight :size="14" />
            <strong>{{ nav.find((item) => item.id === page)?.label || selectedDatabase?.name }}</strong>
          </div>
          <div class="ml-auto flex items-center gap-2">
            <Button variant="ghost" size="icon" :title="dark ? 'Use light theme' : 'Use dark theme'" @click="toggleTheme">
              <Sun v-if="dark" />
              <Moon v-else />
            </Button>
            <Button @click="beginWizard"><Plus /> <span class="hidden sm:inline">Add database</span></Button>
          </div>
        </div>
      </header>

      <Alert v-if="error" variant="destructive" class="rounded-none border-x-0">
        <AlertTriangle />
        <AlertDescription class="flex w-full items-center justify-between gap-3">
          <span>{{ error }}</span>
          <Button variant="ghost" size="icon-xs" class="shrink-0" @click="error = ''"><X /></Button>
        </AlertDescription>
      </Alert>

      <section v-if="loading && !databases.length" class="content" aria-label="Loading">
        <Skeleton class="mb-8 h-16 w-80" />
        <div class="metric-grid">
          <Skeleton v-for="index in 4" :key="index" class="h-36" />
        </div>
        <Skeleton class="h-96" />
      </section>

      <section v-else-if="page === 'overview'" class="content">
        <header class="page-heading split">
          <div>
            <p class="kicker">Live mirror fleet</p>
            <h1>Operations at a glance</h1>
            <p class="muted">Durable source progress, query visibility, and faults on this node.</p>
          </div>
          <Button variant="outline" @click="loadControlPlane"><RefreshCw /> Refresh</Button>
        </header>

        <Alert v-if="alertCount" class="border-amber/40 bg-amber-soft text-amber [&>svg]:text-amber mb-4">
          <AlertTriangle />
          <AlertDescription class="text-amber flex w-full items-center justify-between gap-3">
            <span>
              {{ deadLetters.length }} dead-letter event{{ deadLetters.length === 1 ? '' : 's' }};
              {{ databases.filter((item) => item.state === 'needs_resync').length }} mirror{{ databases.filter((item) => item.state === 'needs_resync').length === 1 ? '' : 's' }} need resync.
            </span>
            <Button variant="outline" size="xs" class="shrink-0" @click="go('activity')">Inspect</Button>
          </AlertDescription>
        </Alert>

        <div class="@container/main grid grid-cols-1 gap-4 *:data-[slot=card]:bg-gradient-to-t *:data-[slot=card]:from-primary/5 *:data-[slot=card]:to-card *:data-[slot=card]:shadow-xs @xl/main:grid-cols-2 @5xl/main:grid-cols-4 dark:*:data-[slot=card]:bg-card">
          <Card class="@container/card">
            <CardHeader>
              <CardDescription>Databases</CardDescription>
              <CardTitle class="text-2xl font-semibold tabular-nums @[250px]/card:text-3xl">{{ databases.length }}</CardTitle>
              <CardAction v-if="activeMirrors">
                <Badge variant="outline">{{ activeMirrors }} live</Badge>
              </CardAction>
            </CardHeader>
            <CardFooter class="flex-col items-start gap-1.5 text-sm">
              <div class="font-medium">{{ activeMirrors }} actively converging</div>
              <div class="text-muted-foreground">Streaming and polling mirrors</div>
            </CardFooter>
          </Card>
          <Card class="@container/card">
            <CardHeader>
              <CardDescription>Rows mirrored</CardDescription>
              <CardTitle class="text-2xl font-semibold tabular-nums @[250px]/card:text-3xl">{{ formatNumber(totalRows) }}</CardTitle>
            </CardHeader>
            <CardFooter class="flex-col items-start gap-1.5 text-sm">
              <div class="font-medium">Deduplicated visible rows</div>
              <div class="text-muted-foreground">Merge-on-read across all mirrors</div>
            </CardFooter>
          </Card>
          <Card class="@container/card">
            <CardHeader>
              <CardDescription>Recent ingest</CardDescription>
              <CardTitle class="text-2xl font-semibold tabular-nums @[250px]/card:text-3xl">{{ formatNumber(activity.slice(0, 20).reduce((sum, run) => sum + run.rows, 0)) }}</CardTitle>
            </CardHeader>
            <CardFooter class="flex-col items-start gap-1.5 text-sm">
              <div class="font-medium">Rows across 20 latest runs</div>
              <div class="text-muted-foreground">Snapshot, stream, and poll work</div>
            </CardFooter>
          </Card>
          <Card class="@container/card">
            <CardHeader>
              <CardDescription>Storage engine</CardDescription>
              <CardTitle class="text-2xl font-semibold tabular-nums @[250px]/card:text-3xl">v1</CardTitle>
              <CardAction>
                <Badge variant="outline" class="tone-positive">Live</Badge>
              </CardAction>
            </CardHeader>
            <CardFooter class="flex-col items-start gap-1.5 text-sm">
              <div class="font-medium">Checksummed columnar blocks</div>
              <div class="text-muted-foreground">Bounded size-tier compaction</div>
            </CardFooter>
          </Card>
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
            <div class="panel-heading"><h2>Database lag posture</h2><Button variant="link" size="sm" @click="go('databases')">View all</Button></div>
            <div v-if="!databases.length" class="empty-state compact-empty">
              <Database :size="24" /><strong>No source connected</strong><span>Add MySQL to begin the first mirror.</span>
              <Button @click="beginWizard">Add database</Button>
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
            <div class="panel-heading"><h2>Latest activity</h2><Button variant="link" size="sm" @click="go('activity')">Open log</Button></div>
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
          <Button @click="beginWizard"><Plus /> Add database</Button>
        </header>
        <article class="panel table-panel">
          <div v-if="!databases.length" class="empty-state">
            <Database :size="30" /><h2>No databases yet</h2><p>Connect a source, inspect its capabilities, and choose the tables to mirror.</p>
            <Button @click="beginWizard">Start the connection wizard</Button>
          </div>
          <Table v-else>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Mode</TableHead>
                <TableHead>State</TableHead>
                <TableHead>Rows</TableHead>
                <TableHead>Last event</TableHead>
                <TableHead><span class="sr-only">Actions</span></TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              <TableRow v-for="database in databases" :key="database.id">
                <TableCell><button class="table-link" @click="openDatabase(database)"><span class="database-glyph">{{ database.name.slice(0, 2).toUpperCase() }}</span><strong>{{ database.name }}</strong></button></TableCell>
                <TableCell><Badge :class="`tone-${modeOf(database) === 'cdc' ? 'positive' : modeOf(database) === 'polling' ? 'warning' : 'neutral'}`">{{ modeOf(database) }}</Badge></TableCell>
                <TableCell><span class="state-label"><span class="event-dot" :class="stateTone(database.state)" />{{ database.state }}</span></TableCell>
                <TableCell class="mono">{{ statuses[database.id]?.rows.toLocaleString() || 0 }}</TableCell>
                <TableCell class="muted">{{ formatDate(database.updated_at) }}</TableCell>
                <TableCell>
                  <div class="row-actions">
                    <Button variant="ghost" size="icon-sm" :title="database.mode === 'paused' ? 'Resume' : 'Pause'" @click="setMode(database, database.mode === 'paused' ? 'auto' : 'paused')">
                      <Play v-if="database.mode === 'paused'" /><Pause v-else />
                    </Button>
                    <Button variant="ghost" size="icon-sm" title="Delete" @click="deleteCandidate = database; deleteText = ''"><Trash2 /></Button>
                  </div>
                </TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </article>
      </section>

      <section v-else-if="page === 'database' && selectedDatabase" class="content">
        <header class="page-heading split database-heading">
          <div>
            <Button variant="link" size="sm" class="mb-2 px-0" @click="go('databases')">Databases /</Button>
            <h1>{{ selectedDatabase.name }}</h1>
            <div class="heading-badges">
              <Badge :class="`tone-${stateTone(selectedDatabase.state)}`">{{ selectedDatabase.state }}</Badge>
              <Badge variant="outline">{{ modeOf(selectedDatabase) }}</Badge>
              <span>{{ statuses[selectedDatabase.id]?.rows.toLocaleString() || 0 }} visible rows</span>
            </div>
          </div>
          <div class="top-actions">
            <Button variant="outline" @click="setMode(selectedDatabase, selectedDatabase.mode === 'paused' ? 'auto' : 'paused')">
              <Play v-if="selectedDatabase.mode === 'paused'" /><Pause v-else />
              {{ selectedDatabase.mode === 'paused' ? 'Resume' : 'Pause' }}
            </Button>
            <Button @click="forceSnapshot"><RefreshCw /> Resnapshot</Button>
          </div>
        </header>

        <Alert v-if="modeOf(selectedDatabase) === 'polling'" class="border-amber/40 bg-amber-soft text-amber [&>svg]:text-amber mb-4">
          <AlertTriangle />
          <AlertDescription class="text-amber">Polling mode has no transaction atomicity: a query can observe part of a source transaction. Intermediate states between polls are lost, and deletes converge on the reconcile interval rather than in seconds. Workloads needing cross-table point-in-time correctness should run on a CDC-capable source.</AlertDescription>
        </Alert>

        <Tabs v-model="detailTab">
          <TabsList class="mb-4" aria-label="Database detail">
            <TabsTrigger v-for="tab in ['tables', 'snapshot', 'replication', 'schema', 'storage', 'settings']" :key="tab" :value="tab" class="capitalize">{{ tab }}</TabsTrigger>
          </TabsList>

          <TabsContent value="tables">
            <article class="panel table-panel">
              <div v-if="!tables.length" class="empty-state compact-empty"><Table2 :size="26" /><strong>No mirrored tables</strong><span>Run a snapshot or revise the include list.</span></div>
              <Table v-else>
                <TableHeader>
                  <TableRow><TableHead>Table</TableHead><TableHead>State</TableHead><TableHead>Rows</TableHead><TableHead>Schema</TableHead><TableHead>Fault</TableHead><TableHead>Action</TableHead></TableRow>
                </TableHeader>
                <TableBody>
                  <TableRow v-for="table in tables" :key="table.name">
                    <TableCell>
                      <strong>{{ table.name }}</strong>
                      <Badge
                        v-if="table.cascade_reconciled"
                        class="tone-warning"
                        title="A source foreign key cascades into this table. MySQL performs cascades inside InnoDB without writing row events, so they cannot reach the replica through CDC; these rows converge on the reconcile interval rather than in seconds."
                      >cascade</Badge>
                    </TableCell>
                    <TableCell><Badge :class="`tone-${stateTone(table.state)}`">{{ table.state }}</Badge></TableCell>
                    <TableCell class="mono">{{ table.rows.toLocaleString() }}</TableCell>
                    <TableCell class="mono">v{{ table.schema_version }}</TableCell>
                    <TableCell class="muted">{{ table.last_error || '—' }}</TableCell>
                    <TableCell>
                      <div class="row-actions">
                        <Button variant="link" size="sm" :disabled="Boolean(tableAction)" @click="runTableAction(table, 'reconcile')">
                          <LoaderCircle v-if="tableAction === `${table.name}:reconcile`" class="spin" /> Reconcile
                        </Button>
                        <Button variant="link" size="sm" :disabled="Boolean(tableAction)" title="Starts a mirror-wide resnapshot because all tables share one source checkpoint" @click="runTableAction(table, 'resync')">
                          <LoaderCircle v-if="tableAction === `${table.name}:resync`" class="spin" /> Resync
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                </TableBody>
              </Table>
            </article>
          </TabsContent>

          <TabsContent value="snapshot">
            <div class="stack">
              <article class="panel progress-overview">
                <div><p class="kicker">Durable publication</p><h2>{{ snapshot?.state || selectedDatabase.state }}</h2><p class="muted">Progress advances only after a chunk and its control-plane checkpoint are durable.</p></div>
                <div class="progress-stat"><strong>{{ snapshot?.tables.reduce((sum, table) => sum + table.rows, 0).toLocaleString() || 0 }}</strong><span>rows published</span></div>
              </article>
              <article class="panel">
                <div v-if="!snapshot?.tables.length" class="empty-state compact-empty"><HardDrive :size="26" /><strong>No snapshot journal</strong><span>Start a snapshot to see per-table progress.</span></div>
                <div v-else class="progress-list">
                  <div v-for="table in snapshot.tables" :key="table.name" class="progress-row">
                    <div><strong>{{ table.name }}</strong><span>{{ table.completed_chunks }}/{{ table.total_chunks }} chunks · {{ table.rows.toLocaleString() }} rows</span></div>
                    <Progress :model-value="snapshotPercent(table)" />
                    <strong class="mono">{{ snapshotPercent(table) }}%</strong>
                  </div>
                </div>
              </article>
            </div>
          </TabsContent>

          <TabsContent value="replication">
            <div class="two-column">
              <article class="panel">
                <div class="panel-heading"><div><p class="kicker">Checkpoint</p><h2>{{ modeOf(selectedDatabase) }}</h2></div><Radio :size="20" /></div>
                <dl class="definition-grid"><div><dt>State</dt><dd>{{ selectedDatabase.state }}</dd></div><div><dt>Poll cadence</dt><dd>{{ selectedDatabase.poll_interval_seconds }}s</dd></div><div><dt>Reconcile</dt><dd>{{ selectedDatabase.reconcile_interval_seconds }}s</dd></div><div><dt>Updated</dt><dd>{{ formatDate(selectedDatabase.updated_at) }}</dd></div></dl>
              </article>
              <article class="panel">
                <div class="panel-heading"><h2>Dead-letter queue</h2><Badge :class="deadLetters.filter((item) => item.database_id === selectedDatabase?.id).length ? 'tone-negative' : 'tone-positive'">{{ deadLetters.filter((item) => item.database_id === selectedDatabase?.id).length }}</Badge></div>
                <div v-if="!deadLetters.filter((item) => item.database_id === selectedDatabase?.id).length" class="empty-state compact-empty"><Check :size="24" /><strong>No rejected events</strong><span>Decoder and storage errors appear here.</span></div>
                <div v-for="record in deadLetters.filter((item) => item.database_id === selectedDatabase?.id)" :key="record.id" class="dlq-card">
                  <strong>{{ record.table || 'database' }}</strong>
                  <p>{{ record.error }}</p>
                  <div class="row-actions">
                    <Button size="sm" :disabled="!record.table" @click="retryDlq(record)"><RefreshCw /> Retry safely</Button>
                    <Button variant="link" size="sm" @click="discardDlq(record)">Discard</Button>
                  </div>
                </div>
              </article>
            </div>
          </TabsContent>

          <TabsContent value="schema">
            <article class="panel">
              <div class="panel-heading"><div><p class="kicker">Replica catalog</p><h2>Schema generations</h2></div><Badge variant="outline">{{ tables.length }} tables</Badge></div>
              <div class="schema-grid"><button v-for="table in tables" :key="table.name" @click="describeTable(table)"><Table2 :size="16" /><span><strong>{{ table.name }}</strong><small>Generation {{ table.schema_version }}</small></span><ChevronRight :size="15" /></button></div>
            </article>
          </TabsContent>

          <TabsContent value="storage">
            <article class="panel">
              <div class="panel-heading"><div><p class="kicker">Columnar footprint</p><h2>Storage posture</h2></div><HardDrive :size="20" /></div>
              <div class="metric-grid three"><div class="metric-card"><span>Visible rows</span><strong>{{ formatNumber(statuses[selectedDatabase.id]?.rows || 0) }}</strong><small>Merge-on-read deduplicated</small></div><div class="metric-card"><span>Schema generations</span><strong>{{ tables.reduce((sum, table) => sum + table.schema_version, 0) }}</strong><small>Stable column IDs</small></div><div class="metric-card"><span>Compaction</span><strong>Auto</strong><small>Bounded size-tier passes</small></div></div>
              <p class="muted panel-note">Exact segment bytes and compression ratios are exported by the operations metrics surface in M8.</p>
            </article>
          </TabsContent>

          <TabsContent value="settings">
            <article class="panel settings-form">
              <div class="panel-heading"><div><p class="kicker">Replication controls</p><h2>Database settings</h2></div></div>
              <div class="grid max-w-xs gap-1.5">
                <Label>Requested mode</Label>
                <Select :model-value="selectedDatabase.mode" @update:model-value="(value) => setMode(selectedDatabase!, value as DatabaseRecord['mode'])">
                  <SelectTrigger class="w-full"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="auto">Auto</SelectItem>
                    <SelectItem value="cdc">CDC</SelectItem>
                    <SelectItem value="polling">Polling</SelectItem>
                    <SelectItem value="paused">Paused</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div class="definition-grid"><div><dt>Poll cadence</dt><dd>{{ selectedDatabase.poll_interval_seconds }} seconds</dd></div><div><dt>Reconciliation</dt><dd>{{ selectedDatabase.reconcile_interval_seconds }} seconds</dd></div><div><dt>Included</dt><dd>{{ selectedDatabase.include_tables.length || 'All tables' }}</dd></div><div><dt>Excluded</dt><dd>{{ selectedDatabase.exclude_tables.length || 'None' }}</dd></div></div>
            </article>
          </TabsContent>
        </Tabs>
      </section>

      <section v-else-if="page === 'wizard'" class="content wizard-page">
        <header class="page-heading"><p class="kicker">Add database</p><h1>Build a live mirror</h1><p class="muted">Connection, capability proof, table selection, then durable handoff.</p></header>
        <ol class="stepper">
          <li v-for="(label, index) in ['Connection', 'Probe', 'Tables', 'Start']" :key="label" :class="{ active: wizard.step === index + 1, complete: wizard.step > index + 1 }"><span>{{ wizard.step > index + 1 ? '✓' : index + 1 }}</span>{{ label }}</li>
        </ol>
        <article class="panel wizard-panel">
          <form v-if="wizard.step === 1" class="wizard-form" @submit.prevent="wizardConnection">
            <div><p class="kicker">01 / Connection</p><h2>Where is MySQL?</h2><p class="muted">The DSN is encrypted before it enters the control-plane database.</p></div>
            <div class="form-grid">
              <div class="grid content-start gap-1.5">
                <Label for="wizard-name">MySQL schema</Label>
                <Input id="wizard-name" v-model="wizard.name" required placeholder="analytics" />
                <small class="text-muted-foreground text-xs">Exact source schema name and case.</small>
              </div>
              <div class="full grid content-start gap-1.5">
                <Label for="wizard-dsn">MySQL DSN</Label>
                <Input id="wizard-dsn" v-model="wizard.dsn" required type="password" placeholder="mysql://pintail:secret@db.internal/analytics" />
              </div>
            </div>
            <p v-if="wizard.error" class="inline-error">{{ wizard.error }}</p>
            <div class="wizard-actions">
              <Button type="button" variant="outline" @click="go('databases')">Cancel</Button>
              <Button type="submit" :disabled="wizard.working"><LoaderCircle v-if="wizard.working" class="spin" /> Test connection <ArrowRight v-if="!wizard.working" /></Button>
            </div>
          </form>
          <div v-else-if="wizard.step === 2" class="wizard-form">
            <div><p class="kicker">02 / Capability probe</p><h2>{{ wizard.serverVersion }}</h2><p class="muted">Pintail checks every invariant required for safe snapshot and stream ownership.</p></div>
            <div v-if="wizard.probe" class="checklist">
              <div v-for="(value, key) in wizard.probe.capabilities" v-show="typeof value === 'boolean'" :key="key"><span :class="value ? 'check-positive' : 'check-negative'"><Check v-if="value" :size="14" /><X v-else :size="14" /></span><strong>{{ String(key).replaceAll('_', ' ') }}</strong><small>{{ value ? 'Pass' : 'Requires remediation' }}</small></div>
            </div>
            <div class="recommendation"><Radio :size="18" /><div><strong>Recommended: {{ wizard.probe?.capabilities.recommended_mode.toUpperCase() }}</strong><span>{{ wizard.probe?.capabilities.reasons.join(' · ') || 'All native replication requirements passed.' }}</span></div></div>
            <div class="grid gap-2">
              <Label>Replication mode</Label>
              <RadioGroup v-model="wizard.mode" class="flex gap-5">
                <div class="flex items-center gap-2"><RadioGroupItem id="wizard-mode-cdc" value="cdc" /><Label for="wizard-mode-cdc">CDC</Label></div>
                <div class="flex items-center gap-2"><RadioGroupItem id="wizard-mode-polling" value="polling" /><Label for="wizard-mode-polling">Polling</Label></div>
              </RadioGroup>
            </div>
            <div class="wizard-actions"><Button variant="outline" @click="wizard.step = 1">Back</Button><Button @click="wizard.step = 3">Choose tables <ArrowRight /></Button></div>
          </div>
          <div v-else-if="wizard.step === 3 && wizard.probe" class="wizard-form">
            <div><p class="kicker">03 / Table selection</p><h2>Choose the analytical surface</h2><p class="muted">PK-less append tables preserve rows but cannot model source updates or deletes.</p></div>
            <div class="table-picker">
              <div v-for="table in wizard.probe.tables" :key="table.name" class="table-picker-row">
                <Checkbox
                  :id="`wizard-pick-${table.name}`"
                  :model-value="wizard.includes.includes(table.name)"
                  @update:model-value="(checked) => toggleInclude(table.name, checked === true)"
                />
                <Label :for="`wizard-pick-${table.name}`" class="grid gap-0"><strong>{{ table.name }}</strong><small>{{ table.estimated_rows?.toLocaleString() || 'Unknown' }} rows · {{ table.engine || 'Unknown engine' }}</small></Label>
                <Badge :class="table.key.mode === 'append_row_id' ? 'tone-warning' : 'tone-positive'">{{ table.key.mode.replace('_', ' ') }}</Badge>
                <AlertTriangle v-if="table.warnings.length" :size="16" />
              </div>
            </div>
            <p v-if="wizard.error" class="inline-error">{{ wizard.error }}</p>
            <div class="wizard-actions"><Button variant="outline" @click="wizard.step = 2">Back</Button><Button :disabled="wizard.working || !wizard.includes.length" @click="finishWizard"><LoaderCircle v-if="wizard.working" class="spin" /> Review & start <ArrowRight v-if="!wizard.working" /></Button></div>
          </div>
          <div v-else class="empty-state"><LoaderCircle class="spin" :size="28" /><h2>Starting the mirror</h2><p>Capturing the source position and establishing resumable chunks.</p></div>
        </article>
      </section>

      <section v-else-if="page === 'sql'" class="content sql-page">
        <header class="page-heading split">
          <div><p class="kicker">Native query engine</p><h1>SQL Console</h1><p class="muted">MySQL dialect over reader-pinned columnar snapshots.</p></div>
          <Select v-model="sqlDatabaseId">
            <SelectTrigger class="min-w-52"><Database :size="15" /><SelectValue placeholder="Choose database" /></SelectTrigger>
            <SelectContent>
              <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
            </SelectContent>
          </Select>
        </header>
        <div v-if="!databases.length" class="panel empty-state"><SquareTerminal :size="30" /><h2>No queryable mirror</h2><p>Add and snapshot a database before opening the console.</p><Button @click="beginWizard">Add database</Button></div>
        <template v-else>
          <article class="panel editor-panel">
            <div class="editor-toolbar"><span>query.sql</span><div><span class="shortcut">⌘ Enter</span><Button size="sm" :disabled="sqlRunning" @click="runSql"><LoaderCircle v-if="sqlRunning" class="spin" /><Play v-else /> Run</Button></div></div>
            <LazySqlEditor v-model="sqlText" @run="runSql" />
          </article>
          <p v-if="sqlError" class="inline-error sql-error">{{ sqlError }}</p>
          <article class="panel result-panel">
            <div class="panel-heading">
              <div><h2>Results</h2><p v-if="sqlResult" class="muted">{{ sqlResult.stats.rows }} rows · {{ sqlResult.stats.duration_ms }} ms · {{ sqlResult.stats.blocks_read }} blocks read / {{ sqlResult.stats.blocks_pruned }} pruned</p></div>
              <div v-if="sqlResult" class="row-actions">
                <Button variant="outline" size="sm" @click="exportResult('csv')">CSV</Button>
                <Button variant="outline" size="sm" @click="exportResult('json')">JSON</Button>
              </div>
            </div>
            <div v-if="!sqlResult" class="empty-state compact-empty"><Search :size="24" /><strong>Run a query</strong><span>Typed fields and physical scan counters appear here.</span></div>
            <Table v-else class="result-scroll">
              <TableHeader>
                <TableRow>
                  <TableHead v-for="field in sqlResult.fields" :key="field.name"><span>{{ field.name }}</span><small>{{ typeof field.data_type === 'string' ? field.data_type : 'typed' }}</small></TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                <TableRow v-for="(row, rowIndex) in sqlResult.rows" :key="rowIndex">
                  <TableCell v-for="(value, valueIndex) in row" :key="valueIndex" :class="{ null: value === null }">{{ displayValue(value) }}</TableCell>
                </TableRow>
              </TableBody>
            </Table>
          </article>
        </template>
      </section>

      <section v-else-if="page === 'activity'" class="content">
        <header class="page-heading split">
          <div><p class="kicker">Durable work log</p><h1>Activity</h1><p class="muted">Snapshot, stream, poll, and repair outcomes from control-plane records.</p></div>
          <Select
            :model-value="activityDatabase || 'all'"
            @update:model-value="(value) => activityDatabase = value === 'all' ? '' : String(value)"
          >
            <SelectTrigger class="min-w-52"><Database :size="15" /><SelectValue placeholder="All databases" /></SelectTrigger>
            <SelectContent>
              <SelectItem value="all">All databases</SelectItem>
              <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
            </SelectContent>
          </Select>
        </header>
        <article class="panel table-panel">
          <div v-if="!filteredActivity.length" class="empty-state"><Activity :size="28" /><h2>No matching activity</h2><p>Completed and failed replication work appears after the first snapshot.</p></div>
          <Table v-else>
            <TableHeader>
              <TableRow><TableHead>Started</TableHead><TableHead>Database</TableHead><TableHead>Kind</TableHead><TableHead>Status</TableHead><TableHead>Rows</TableHead><TableHead>Bytes</TableHead><TableHead>Duration</TableHead></TableRow>
            </TableHeader>
            <TableBody>
              <TableRow v-for="record in filteredActivity" :key="record.id">
                <TableCell class="muted">{{ formatDate(record.started_at) }}</TableCell>
                <TableCell><strong>{{ databases.find((item) => item.id === record.database_id)?.name || record.database_id }}</strong><small v-if="record.table">{{ record.table }}</small></TableCell>
                <TableCell>{{ record.kind }}</TableCell>
                <TableCell><Badge :class="`tone-${stateTone(record.status)}`">{{ record.status }}</Badge></TableCell>
                <TableCell class="mono">{{ record.rows.toLocaleString() }}</TableCell>
                <TableCell class="mono">{{ formatBytes(record.bytes) }}</TableCell>
                <TableCell class="mono">{{ record.duration_ms === null ? '—' : `${record.duration_ms} ms` }}</TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </article>
        <article v-if="deadLetters.length" class="panel dlq-panel">
          <div class="panel-heading"><div><p class="kicker">Requires judgment</p><h2>Dead-letter queue</h2></div><Badge class="tone-negative">{{ deadLetters.length }}</Badge></div>
          <div class="dlq-grid">
            <div v-for="record in deadLetters" :key="record.id" class="dlq-card">
              <div><strong>{{ record.table || 'Database event' }}</strong><span>{{ formatDate(record.created_at) }}</span></div>
              <p>{{ record.error }}</p>
              <pre>{{ JSON.stringify(record.event, null, 2) }}</pre>
              <div class="row-actions">
                <Button size="sm" :disabled="!record.table" @click="retryDlq(record)"><RefreshCw /> Retry safely</Button>
                <Button variant="destructive" size="sm" @click="discardDlq(record)">Discard</Button>
              </div>
            </div>
          </div>
        </article>
      </section>

      <section v-else-if="page === 'keys'" class="content">
        <header class="page-heading split">
          <div><p class="kicker">Database-scoped access</p><h1>API Keys</h1><p class="muted">Secrets are SHA-256 hash-only and shown once.</p></div>
          <Select v-model="keyDatabaseId" @update:model-value="loadKeys">
            <SelectTrigger class="min-w-52"><Database :size="15" /><SelectValue placeholder="Choose database" /></SelectTrigger>
            <SelectContent>
              <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
            </SelectContent>
          </Select>
        </header>
        <article class="panel key-create">
          <div><h2>Create a key</h2><p class="muted">Use a narrow scope for each application.</p></div>
          <div class="grid content-start gap-1.5">
            <Label for="key-name">Name</Label>
            <Input id="key-name" v-model="keyForm.name" placeholder="Metabase production" />
          </div>
          <div class="grid content-start gap-2">
            <Label>Scopes</Label>
            <div class="flex flex-wrap gap-4 pt-1.5">
              <div class="flex items-center gap-2"><Checkbox id="scope-read" :model-value="keyForm.scopes.includes('read')" @update:model-value="(checked) => toggleScope('read', checked === true)" /><Label for="scope-read">Read metadata</Label></div>
              <div class="flex items-center gap-2"><Checkbox id="scope-query" :model-value="keyForm.scopes.includes('query')" @update:model-value="(checked) => toggleScope('query', checked === true)" /><Label for="scope-query">Run queries</Label></div>
            </div>
          </div>
          <Button :disabled="!keyDatabaseId || !keyForm.name || !keyForm.scopes.length" @click="createKey"><Plus /> Create</Button>
        </article>
        <div v-if="revealedSecret" class="secret-banner">
          <AlertTriangle :size="18" />
          <div><strong>Copy this secret now. It cannot be recovered.</strong><code>{{ revealedSecret }}</code></div>
          <Button variant="ghost" size="icon-sm" class="shrink-0" @click="copy(revealedSecret)"><Copy /></Button>
          <Button variant="ghost" size="icon-sm" class="shrink-0" @click="revealedSecret = ''"><X /></Button>
        </div>
        <article class="panel table-panel">
          <div v-if="!keys.length" class="empty-state"><KeyRound :size="28" /><h2>No keys for this database</h2><p>Create one for the HTTP API or MySQL wire clients.</p></div>
          <Table v-else>
            <TableHeader>
              <TableRow><TableHead>Name</TableHead><TableHead>Scopes</TableHead><TableHead>Status</TableHead><TableHead>Last used</TableHead><TableHead>Created</TableHead><TableHead><span class="sr-only">Actions</span></TableHead></TableRow>
            </TableHeader>
            <TableBody>
              <TableRow v-for="key in keys" :key="key.id">
                <TableCell><strong>{{ key.name }}</strong><small>{{ key.id }}</small></TableCell>
                <TableCell><Badge v-for="scope in key.scopes" :key="scope" variant="outline" class="mr-1">{{ scope }}</Badge></TableCell>
                <TableCell><Badge :class="key.enabled ? 'tone-positive' : 'tone-neutral'">{{ key.enabled ? 'enabled' : 'disabled' }}</Badge></TableCell>
                <TableCell class="muted">{{ formatDate(key.last_used_at) }}</TableCell>
                <TableCell class="muted">{{ formatDate(key.created_at) }}</TableCell>
                <TableCell>
                  <div class="row-actions">
                    <Button variant="link" size="sm" @click="toggleKey(key)">{{ key.enabled ? 'Disable' : 'Enable' }}</Button>
                    <Button variant="ghost" size="icon-sm" @click="deleteKey(key)"><Trash2 /></Button>
                  </div>
                </TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </article>
      </section>

      <section v-else-if="page === 'backups'" class="content">
        <header class="page-heading split">
          <div><p class="kicker">Recovery plane</p><h1>Backups</h1><p class="muted">Checksum-verified manifests, immutable segments, and control-plane state restore side-by-side.</p></div>
          <Select v-model="backupDatabaseId" @update:model-value="() => loadBackups()">
            <SelectTrigger class="min-w-52"><Database :size="15" /><SelectValue placeholder="Choose database" /></SelectTrigger>
            <SelectContent>
              <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
            </SelectContent>
          </Select>
        </header>
        <div class="two-column">
          <article class="panel settings-form">
            <div class="panel-heading"><div><p class="kicker">S3-compatible destination</p><h2>Backup configuration</h2></div><Badge :class="backupConfigLoaded ? 'tone-positive' : 'tone-neutral'">{{ backupConfigLoaded ? 'Configured' : 'Not configured' }}</Badge></div>
            <div class="form-grid">
              <div class="grid content-start gap-1.5"><Label for="backup-bucket">Bucket</Label><Input id="backup-bucket" v-model="backupForm.bucket" autocomplete="off" placeholder="analytics-backups" /></div>
              <div class="grid content-start gap-1.5"><Label for="backup-prefix">Object prefix</Label><Input id="backup-prefix" v-model="backupForm.prefix" autocomplete="off" placeholder="pintail/production" /></div>
              <div class="full grid content-start gap-1.5"><Label for="backup-endpoint">Endpoint <small class="text-muted-foreground font-normal">optional for AWS</small></Label><Input id="backup-endpoint" v-model="backupForm.endpoint" autocomplete="url" placeholder="http://minio.internal:9000" /></div>
              <div class="grid content-start gap-1.5"><Label for="backup-region">Region</Label><Input id="backup-region" v-model="backupForm.region" autocomplete="off" placeholder="us-east-1" /></div>
              <div class="grid content-start gap-1.5">
                <Label>Schedule cadence</Label>
                <Select :model-value="backupForm.scheduleMinutes" @update:model-value="(value) => backupForm.scheduleMinutes = Number(value)">
                  <SelectTrigger class="w-full"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem :value="60">Hourly</SelectItem>
                    <SelectItem :value="360">Every 6 hours</SelectItem>
                    <SelectItem :value="1440">Daily</SelectItem>
                    <SelectItem :value="10080">Weekly</SelectItem>
                  </SelectContent>
                </Select>
              </div>
              <div class="grid content-start gap-1.5"><Label for="backup-access-key">Access key ID</Label><Input id="backup-access-key" v-model="backupForm.accessKeyId" autocomplete="off" placeholder="Leave blank to preserve" /></div>
              <div class="grid content-start gap-1.5"><Label for="backup-secret-key">Secret access key</Label><Input id="backup-secret-key" v-model="backupForm.secretAccessKey" type="password" autocomplete="new-password" placeholder="Leave blank to preserve" /></div>
            </div>
            <div class="setting-row">
              <span><strong>Scheduled backups</strong><small>Runs after the next healthy supervised cycle when due.</small></span>
              <Switch :model-value="backupForm.enabled" @update:model-value="(value) => backupForm.enabled = value === true" />
            </div>
            <Button :disabled="backupLoading || !backupDatabaseId || !backupForm.bucket.trim() || !backupForm.prefix.trim()" @click="saveBackupConfig"><LoaderCircle v-if="backupLoading" class="spin" /><HardDrive v-else /> Save destination</Button>
            <p class="muted panel-note">Prefix validation prevents accidental broad writes; it is not a tenant-isolation boundary. Use bucket IAM for isolation.</p>
          </article>
          <article class="panel backup-operations">
            <div class="panel-heading"><div><p class="kicker">Manual recovery point</p><h2>Backup now</h2></div><Archive :size="19" /></div>
            <p class="muted">The first run is full. Later runs reuse unchanged immutable segment objects unless you force a new full chain.</p>
            <div class="row-actions">
              <Button :disabled="backupLoading || !backupConfigLoaded" @click="runBackup(false)"><Play /> Backup now</Button>
              <Button variant="outline" :disabled="backupLoading || !backupConfigLoaded" @click="runBackup(true)"><RefreshCw /> Force full</Button>
            </div>
            <Separator />
            <div><p class="kicker">Side-by-side restore</p><h2>Restore as new database</h2></div>
            <div class="grid gap-1.5">
              <Label>Completed backup</Label>
              <Select v-model="restoreBackupId">
                <SelectTrigger class="w-full"><SelectValue placeholder="Choose recovery point" /></SelectTrigger>
                <SelectContent>
                  <SelectItem v-for="backup in backups.filter((item) => item.status === 'completed')" :key="backup.id" :value="backup.id">{{ formatDate(backup.completed_at) }} · {{ backup.kind }}</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div class="grid gap-1.5">
              <Label for="restore-name">New database name</Label>
              <Input id="restore-name" v-model="restoreName" placeholder="analytics recovery" />
            </div>
            <Button variant="outline" :disabled="backupLoading || !restoreBackupId || !restoreName.trim()" @click="restoreSelectedBackup"><HardDrive /> Verify and restore</Button>
            <p class="muted panel-note">Restore never overwrites a live mirror. The new database is detached from ingestion until new source credentials are supplied.</p>
          </article>
        </div>
        <article class="panel table-panel backup-history">
          <div class="panel-heading p-4 pb-0"><div><p class="kicker">Durable audit</p><h2>Backup history</h2></div><Button variant="ghost" size="icon" :disabled="backupLoading" aria-label="Refresh backup history" @click="loadBackups()"><RefreshCw /></Button></div>
          <div v-if="!backups.length" class="empty-state compact-empty"><Archive :size="26" /><strong>No backup artifacts</strong><span>Save a destination, then create the first full recovery point.</span></div>
          <Table v-else>
            <TableHeader>
              <TableRow><TableHead>Started</TableHead><TableHead>Kind</TableHead><TableHead>Status</TableHead><TableHead>Objects</TableHead><TableHead>Uploaded</TableHead><TableHead>Chain</TableHead><TableHead>Error</TableHead></TableRow>
            </TableHeader>
            <TableBody>
              <TableRow v-for="backup in backups" :key="backup.id">
                <TableCell><strong>{{ formatDate(backup.started_at) }}</strong><small class="mono">{{ backup.id }}</small></TableCell>
                <TableCell><Badge :class="backup.kind === 'full' ? 'tone-positive' : 'tone-neutral'">{{ backup.kind }}</Badge></TableCell>
                <TableCell><Badge :class="`tone-${stateTone(backup.status)}`">{{ backup.status }}</Badge></TableCell>
                <TableCell class="mono">{{ backup.object_count }}</TableCell>
                <TableCell class="mono">{{ formatBytes(backup.bytes) }}</TableCell>
                <TableCell><span v-if="backup.parent_id" class="mono">{{ backup.parent_id }}</span><span v-else class="muted">root</span></TableCell>
                <TableCell class="backup-error">{{ backup.error || '—' }}</TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </article>
      </section>

      <section v-else-if="page === 'settings'" class="content">
        <header class="page-heading"><p class="kicker">Node policy</p><h1>Settings</h1><p class="muted">Operator identity, network surfaces, and local presentation.</p></header>
        <div class="settings-grid">
          <article class="panel"><div class="panel-heading"><div><p class="kicker">Operator</p><h2>Current session</h2></div><Server :size="19" /></div><dl class="definition-grid"><div><dt>Subject</dt><dd class="mono">{{ session.subject }}</dd></div><div><dt>Role</dt><dd>{{ session.role }}</dd></div><div><dt>Scopes</dt><dd>{{ session.scopes.join(', ') }}</dd></div><div><dt>Session</dt><dd>12-hour signed JWT</dd></div></dl></article>
          <article class="panel">
            <div class="panel-heading"><div><p class="kicker">Appearance</p><h2>Interface</h2></div><Button variant="ghost" size="icon" @click="toggleTheme"><Sun v-if="dark" /><Moon v-else /></Button></div>
            <div class="setting-row">
              <span><strong>Dark instrument panel</strong><small>Stored only in this browser.</small></span>
              <Switch :model-value="dark" @update:model-value="() => toggleTheme()" />
            </div>
          </article>
          <article class="panel wire-status"><div class="panel-heading"><div><p class="kicker">MySQL wire</p><h2>Client endpoint</h2></div><Badge :class="nodeStatus?.wire.enabled ? 'tone-positive' : 'tone-negative'">{{ nodeStatus?.wire.enabled ? 'Live' : 'Unavailable' }}</Badge></div><div class="endpoint-line"><span class="endpoint-pulse" :class="{ live: nodeStatus?.wire.enabled }" /><code>{{ nodeStatus?.wire.bind || 'Endpoint unavailable' }}</code></div><dl class="definition-grid"><div><dt>Mode</dt><dd>Read-only</dd></div><div><dt>Authentication</dt><dd>Database API key</dd></div><div><dt>Username</dt><dd>Database name</dd></div><div><dt>Protocol</dt><dd>MySQL native</dd></div></dl></article>
          <article class="panel"><div class="panel-heading"><div><p class="kicker">Telemetry</p><h2>Operations</h2></div><Badge class="tone-positive">Live</Badge></div><dl class="definition-grid"><div><dt>Metrics</dt><dd><a href="/metrics" target="_blank">/metrics</a></dd></div><div><dt>Format</dt><dd>Prometheus text</dd></div><div><dt>Supervisor</dt><dd>Isolated per database</dd></div><div><dt>Recovery</dt><dd>Scheduled + manual</dd></div></dl></article>
        </div>
      </section>

      <section v-else-if="page === 'connect'" class="content">
        <header class="page-heading"><p class="kicker">Client handoff</p><h1>Connect to Pintail</h1><p class="muted">The database name is the username; its scoped API key is the password.</p></header>
        <form class="panel connect-controls" @submit.prevent>
          <div class="grid content-start gap-1.5">
            <Label>Database</Label>
            <Select v-model="keyDatabaseId">
              <SelectTrigger class="w-full"><SelectValue placeholder="Choose database" /></SelectTrigger>
              <SelectContent>
                <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
              </SelectContent>
            </Select>
          </div>
          <div class="grid content-start gap-1.5"><Label for="connect-host">Client host</Label><Input id="connect-host" v-model="connectHost" autocomplete="url" /></div>
          <div class="grid content-start gap-1.5"><Label for="connect-port">Wire port</Label><Input id="connect-port" v-model="connectPort" inputmode="numeric" /></div>
          <div class="grid content-start gap-1.5"><Label for="connect-key">Query-scoped API key</Label><Input id="connect-key" v-model="connectKey" type="password" autocomplete="off" /></div>
        </form>
        <div class="protocol-note">
          <Radio :size="17" />
          <div><strong>Native challenge, no stored plaintext.</strong><span>Use MySQL 8.4, mysql2, PyMySQL, DBeaver, or Metabase. Oracle's MySQL 9.x CLI removed its native-password client plugin.</span></div>
          <Button variant="link" size="sm" class="whitespace-nowrap" @click="go('keys')">Create or rotate key <ArrowRight /></Button>
        </div>
        <div class="snippet-grid">
          <article v-for="kind in (['mysql', 'node', 'python'] as const)" :key="kind" class="panel snippet">
            <div class="panel-heading"><h2>{{ kind === 'mysql' ? 'MySQL CLI' : kind === 'node' ? 'Node.js' : 'Python' }}</h2><Button variant="ghost" size="icon" @click="copy(connectSnippet(kind))"><Copy /></Button></div>
            <pre>{{ connectSnippet(kind) }}</pre>
          </article>
          <article class="panel snippet"><div class="panel-heading"><h2>DBeaver / Metabase</h2><CircleHelp :size="17" /></div><dl class="definition-grid"><div><dt>Driver</dt><dd>MySQL 8</dd></div><div><dt>Host / port</dt><dd>{{ connectHost }}:{{ connectPort }}</dd></div><div><dt>Database / user</dt><dd>{{ selectedConnectDatabase?.name || 'analytics' }}</dd></div><div><dt>Password</dt><dd>Query-scoped API key</dd></div></dl><p class="muted panel-note">Keep SSL disabled for a loopback endpoint. Terminate TLS at your private ingress when clients connect across a network.</p></article>
        </div>
      </section>
    </SidebarInset>

    <Dialog :open="Boolean(deleteCandidate)" @update:open="closeDeleteDialog">
      <DialogContent>
        <DialogHeader>
          <div class="bg-red-soft text-red mb-2 flex size-11 items-center justify-center rounded-md"><Trash2 :size="20" /></div>
          <DialogTitle>Remove {{ deleteCandidate?.name }}?</DialogTitle>
          <DialogDescription>The source configuration is deleted. Mirrored storage is retained for manual recovery.</DialogDescription>
        </DialogHeader>
        <div class="grid gap-1.5">
          <Label for="delete-confirm">Type <strong>{{ deleteCandidate?.name }}</strong> to confirm</Label>
          <Input id="delete-confirm" v-model="deleteText" autocomplete="off" />
        </div>
        <DialogFooter>
          <Button variant="outline" @click="closeDeleteDialog(false)">Cancel</Button>
          <Button variant="destructive" :disabled="deleteText !== deleteCandidate?.name" @click="removeDatabase">Remove database</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  </SidebarProvider>
</template>
