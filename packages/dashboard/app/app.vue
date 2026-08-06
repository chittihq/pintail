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
  bodyAttrs: { class: 'min-h-screen' },
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

function dotToneClass(tone: string) {
  if (tone === 'positive') return 'bg-green'
  if (tone === 'warning') return 'bg-amber'
  if (tone === 'negative') return 'bg-destructive'
  return 'bg-muted-foreground'
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

  <div v-if="booting" class="text-muted-foreground flex min-h-svh items-center justify-center gap-3 font-mono text-xs tracking-wide uppercase" aria-live="polite">
    <div class="bg-primary text-primary-foreground grid size-8 place-items-center font-mono text-[0.65rem] font-extrabold">PT</div>
    <LoaderCircle class="animate-spin" :size="20" />
    <span>Opening control plane</span>
  </div>

  <main v-else-if="!session" class="bg-muted flex min-h-svh items-center justify-center p-6 md:p-10">
    <div class="w-full max-w-4xl">
      <Card class="overflow-hidden p-0">
        <CardContent class="grid p-0 md:grid-cols-2">
          <form class="p-6 md:p-8" @submit.prevent="submitAuth">
            <div class="flex flex-col gap-6">
              <div class="flex flex-col items-center gap-2 text-center">
                <span class="bg-primary text-primary-foreground grid size-8 place-items-center font-mono text-[0.65rem] font-extrabold">PT</span>
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
              <p v-if="error" class="text-destructive text-sm">{{ error }}</p>
              <Button type="submit" class="w-full" :disabled="authenticating">
                <LoaderCircle v-if="authenticating" class="animate-spin" />
                {{ authMode === 'setup' ? 'Initialize Pintail' : 'Sign in' }}
                <ArrowRight v-if="!authenticating" />
              </Button>
              <p class="text-muted-foreground text-center text-xs">Credentials stay on this Pintail node · Argon2id protected</p>
            </div>
          </form>
          <aside class="relative hidden min-h-[22rem] place-items-center overflow-hidden bg-neutral-950 text-neutral-300 md:grid" aria-hidden="true">
            <div class="grid w-[min(70%,44rem)] grid-cols-4 gap-2 p-8 [transform:perspective(60rem)_rotateX(54deg)_rotateZ(-28deg)]">
              <span
                v-for="index in 28"
                :key="index"
                class="min-h-20 border border-neutral-700 bg-neutral-800 shadow-[0_1.2rem_2rem_rgba(0,0,0,0.17)]"
                :class="{ 'border-neutral-100 bg-neutral-100': [7, 14, 21, 22].includes(index) }"
              />
            </div>
            <div class="absolute bottom-8 left-8 flex items-center gap-3 font-mono text-xs tracking-wide">
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
            <SidebarMenuButton size="lg" class="data-[slot=sidebar-menu-button]:!p-1.5 text-base font-extrabold tracking-tight" @click="go('overview')">
              <span class="bg-primary text-primary-foreground grid size-7 shrink-0 place-items-center font-mono text-[0.6rem] font-extrabold">PT</span>
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
                      <span class="size-2 shrink-0 rounded-full" :class="error ? 'bg-destructive' : 'bg-green'" />
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
          <div class="text-muted-foreground flex items-center gap-1.5 text-xs">
            <span>Control plane</span>
            <ChevronRight :size="14" />
            <strong class="text-foreground">{{ nav.find((item) => item.id === page)?.label || selectedDatabase?.name }}</strong>
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

      <section v-if="loading && !databases.length" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6" aria-label="Loading">
        <Skeleton class="mb-8 h-16 w-80" />
        <div class="mb-4 grid grid-cols-1 gap-4 sm:grid-cols-2 xl:grid-cols-4">
          <Skeleton v-for="index in 4" :key="index" class="h-36" />
        </div>
        <Skeleton class="h-96" />
      </section>

      <section v-else-if="page === 'overview'" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
        <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
          <div>
            <p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Live mirror fleet</p>
            <h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Operations at a glance</h1>
            <p class="text-muted-foreground mt-1.5">Durable source progress, query visibility, and faults on this node.</p>
          </div>
          <Button variant="outline" @click="loadControlPlane"><RefreshCw /> Refresh</Button>
        </header>

        <Alert v-if="alertCount" variant="destructive" class="mb-4">
          <AlertTriangle />
          <AlertDescription class="flex w-full items-center justify-between gap-3">
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

        <Card class="my-4 p-5">
          <div class="mb-5 flex flex-wrap items-center justify-between gap-3">
            <div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Signature path</p><h2 class="text-base font-semibold">Source → snapshot → stream</h2></div>
            <Badge variant="outline">Durable boundaries only</Badge>
          </div>
          <div class="grid grid-cols-[minmax(7rem,auto)_minmax(3rem,1fr)_minmax(7rem,auto)_minmax(3rem,1fr)_minmax(7rem,auto)] items-center gap-3 max-sm:grid-cols-1">
            <div class="text-muted-foreground grid justify-items-center gap-1 text-center"><Server :size="19" /><strong class="text-foreground text-sm">Source</strong><span class="font-mono text-[0.6rem]">{{ databases.length }} configured</span></div>
            <div class="bg-border h-px overflow-hidden max-sm:mx-auto max-sm:h-8 max-sm:w-px"><span class="bg-foreground block h-full transition-[width]" :style="{ width: databases.length ? '100%' : '0%' }" /></div>
            <div class="text-muted-foreground grid justify-items-center gap-1 text-center"><HardDrive :size="19" /><strong class="text-foreground text-sm">Snapshot</strong><span class="font-mono text-[0.6rem]">{{ databases.filter((item) => item.state === 'snapshotting').length }} running</span></div>
            <div class="bg-border h-px overflow-hidden max-sm:mx-auto max-sm:h-8 max-sm:w-px"><span class="bg-foreground block h-full transition-[width]" :style="{ width: activeMirrors ? '100%' : '0%' }" /></div>
            <div class="text-muted-foreground grid justify-items-center gap-1 text-center"><Radio :size="19" /><strong class="text-foreground text-sm">Stream</strong><span class="font-mono text-[0.6rem]">{{ activeMirrors }} live</span></div>
          </div>
        </Card>

        <div class="grid gap-4 md:grid-cols-2">
          <Card class="p-4">
            <div class="mb-4 flex items-center justify-between gap-3"><h2 class="text-base font-semibold">Database lag posture</h2><Button variant="link" size="sm" @click="go('databases')">View all</Button></div>
            <div v-if="!databases.length" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 text-center">
              <Database :size="24" /><strong class="text-foreground">No source connected</strong><span class="max-w-sm text-sm">Add MySQL to begin the first mirror.</span>
              <Button @click="beginWizard">Add database</Button>
            </div>
            <div v-else class="divide-y">
              <button v-for="database in databases" :key="database.id" class="hover:bg-accent flex w-full items-center gap-3 py-2.5 text-left" @click="openDatabase(database)">
                <span class="bg-accent text-accent-foreground grid size-8 shrink-0 place-items-center rounded-md border font-mono text-[0.58rem] font-bold">{{ database.name.slice(0, 2).toUpperCase() }}</span>
                <span class="grid flex-1 min-w-0"><strong class="truncate">{{ database.name }}</strong><small class="text-muted-foreground text-xs">{{ statuses[database.id]?.rows.toLocaleString() || 0 }} rows</small></span>
                <Badge :class="`tone-${stateTone(database.state)}`">{{ modeOf(database) }}</Badge>
                <ChevronRight :size="15" class="text-muted-foreground shrink-0" />
              </button>
            </div>
          </Card>
          <Card class="p-4">
            <div class="mb-4 flex items-center justify-between gap-3"><h2 class="text-base font-semibold">Latest activity</h2><Button variant="link" size="sm" @click="go('activity')">Open log</Button></div>
            <div v-if="!activity.length" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 text-center"><Activity :size="24" /><strong class="text-foreground">No sync runs yet</strong><span class="max-w-sm text-sm">Snapshot and replication work appears here.</span></div>
            <ol v-else class="divide-y">
              <li v-for="record in activity.slice(0, 6)" :key="record.id" class="grid grid-cols-[auto_1fr_auto] items-center gap-3 py-2.5">
                <span class="size-2 shrink-0 rounded-full" :class="dotToneClass(stateTone(record.status))" />
                <div class="grid min-w-0 gap-0.5"><strong class="text-sm capitalize">{{ record.kind }}</strong><span class="text-muted-foreground truncate text-xs">{{ databases.find((item) => item.id === record.database_id)?.name || record.database_id }}{{ record.table ? ` · ${record.table}` : '' }}</span></div>
                <time class="text-muted-foreground font-mono text-xs">{{ formatDate(record.started_at) }}</time>
              </li>
            </ol>
          </Card>
        </div>
      </section>

      <section v-else-if="page === 'databases'" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
        <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
          <div><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Source registry</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Databases</h1><p class="text-muted-foreground mt-1.5">Every mirror has its own state, checkpoint, and failure boundary.</p></div>
          <Button @click="beginWizard"><Plus /> Add database</Button>
        </header>
        <Card class="overflow-hidden p-0">
          <div v-if="!databases.length" class="text-muted-foreground grid min-h-80 place-content-center justify-items-center gap-2 p-6 text-center">
            <Database :size="30" /><h2 class="text-foreground font-semibold">No databases yet</h2><p class="max-w-md text-sm">Connect a source, inspect its capabilities, and choose the tables to mirror.</p>
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
                <TableCell><button class="flex items-center gap-2.5" @click="openDatabase(database)"><span class="bg-accent text-accent-foreground grid size-8 place-items-center rounded-md border font-mono text-[0.58rem] font-bold">{{ database.name.slice(0, 2).toUpperCase() }}</span><strong>{{ database.name }}</strong></button></TableCell>
                <TableCell><Badge :class="`tone-${modeOf(database) === 'cdc' ? 'positive' : modeOf(database) === 'polling' ? 'warning' : 'neutral'}`">{{ modeOf(database) }}</Badge></TableCell>
                <TableCell><span class="flex items-center gap-2 capitalize"><span class="size-2 shrink-0 rounded-full" :class="dotToneClass(stateTone(database.state))" />{{ database.state }}</span></TableCell>
                <TableCell class="font-mono">{{ statuses[database.id]?.rows.toLocaleString() || 0 }}</TableCell>
                <TableCell class="text-muted-foreground">{{ formatDate(database.updated_at) }}</TableCell>
                <TableCell>
                  <div class="flex items-center gap-1">
                    <Button variant="ghost" size="icon-sm" :title="database.mode === 'paused' ? 'Resume' : 'Pause'" @click="setMode(database, database.mode === 'paused' ? 'auto' : 'paused')">
                      <Play v-if="database.mode === 'paused'" /><Pause v-else />
                    </Button>
                    <Button variant="ghost" size="icon-sm" title="Delete" @click="deleteCandidate = database; deleteText = ''"><Trash2 /></Button>
                  </div>
                </TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </Card>
      </section>

      <section v-else-if="page === 'database' && selectedDatabase" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
        <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
          <div>
            <Button variant="link" size="sm" class="mb-2 px-0" @click="go('databases')">Databases /</Button>
            <h1 class="text-2xl font-bold tracking-tight sm:text-3xl">{{ selectedDatabase.name }}</h1>
            <div class="text-muted-foreground mt-3 flex items-center gap-2 text-sm">
              <Badge :class="`tone-${stateTone(selectedDatabase.state)}`">{{ selectedDatabase.state }}</Badge>
              <Badge variant="outline">{{ modeOf(selectedDatabase) }}</Badge>
              <span>{{ statuses[selectedDatabase.id]?.rows.toLocaleString() || 0 }} visible rows</span>
            </div>
          </div>
          <div class="flex items-center gap-2">
            <Button variant="outline" @click="setMode(selectedDatabase, selectedDatabase.mode === 'paused' ? 'auto' : 'paused')">
              <Play v-if="selectedDatabase.mode === 'paused'" /><Pause v-else />
              {{ selectedDatabase.mode === 'paused' ? 'Resume' : 'Pause' }}
            </Button>
            <Button @click="forceSnapshot"><RefreshCw /> Resnapshot</Button>
          </div>
        </header>

        <Alert v-if="modeOf(selectedDatabase) === 'polling'" variant="destructive" class="mb-4">
          <AlertTriangle />
          <AlertDescription>Polling mode has no transaction atomicity: a query can observe part of a source transaction. Intermediate states between polls are lost, and deletes converge on the reconcile interval rather than in seconds. Workloads needing cross-table point-in-time correctness should run on a CDC-capable source.</AlertDescription>
        </Alert>

        <Tabs v-model="detailTab">
          <TabsList class="mb-4" aria-label="Database detail">
            <TabsTrigger v-for="tab in ['tables', 'snapshot', 'replication', 'schema', 'storage', 'settings']" :key="tab" :value="tab" class="capitalize">{{ tab }}</TabsTrigger>
          </TabsList>

          <TabsContent value="tables">
            <Card class="overflow-hidden p-0">
              <div v-if="!tables.length" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 text-center"><Table2 :size="26" /><strong class="text-foreground">No mirrored tables</strong><span class="max-w-sm text-sm">Run a snapshot or revise the include list.</span></div>
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
                        class="tone-warning ml-1.5"
                        title="A source foreign key cascades into this table. MySQL performs cascades inside InnoDB without writing row events, so they cannot reach the replica through CDC; these rows converge on the reconcile interval rather than in seconds."
                      >cascade</Badge>
                    </TableCell>
                    <TableCell><Badge :class="`tone-${stateTone(table.state)}`">{{ table.state }}</Badge></TableCell>
                    <TableCell class="font-mono">{{ table.rows.toLocaleString() }}</TableCell>
                    <TableCell class="font-mono">v{{ table.schema_version }}</TableCell>
                    <TableCell class="text-muted-foreground">{{ table.last_error || '—' }}</TableCell>
                    <TableCell>
                      <div class="flex items-center gap-1">
                        <Button variant="link" size="sm" :disabled="Boolean(tableAction)" @click="runTableAction(table, 'reconcile')">
                          <LoaderCircle v-if="tableAction === `${table.name}:reconcile`" class="animate-spin" /> Reconcile
                        </Button>
                        <Button variant="link" size="sm" :disabled="Boolean(tableAction)" title="Starts a mirror-wide resnapshot because all tables share one source checkpoint" @click="runTableAction(table, 'resync')">
                          <LoaderCircle v-if="tableAction === `${table.name}:resync`" class="animate-spin" /> Resync
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                </TableBody>
              </Table>
            </Card>
          </TabsContent>

          <TabsContent value="snapshot">
            <div class="grid gap-4">
              <Card class="grid grid-cols-[1fr_auto] items-center gap-8 p-5 max-sm:grid-cols-1">
                <div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Durable publication</p><h2 class="text-base font-semibold capitalize">{{ snapshot?.state || selectedDatabase.state }}</h2><p class="text-muted-foreground mt-1.5 text-sm">Progress advances only after a chunk and its control-plane checkpoint are durable.</p></div>
                <div class="grid min-w-44 justify-items-end"><strong class="text-3xl font-bold tracking-tight">{{ snapshot?.tables.reduce((sum, table) => sum + table.rows, 0).toLocaleString() || 0 }}</strong><span class="text-muted-foreground text-xs">rows published</span></div>
              </Card>
              <Card class="p-4">
                <div v-if="!snapshot?.tables.length" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 text-center"><HardDrive :size="26" /><strong class="text-foreground">No snapshot journal</strong><span class="max-w-sm text-sm">Start a snapshot to see per-table progress.</span></div>
                <div v-else class="divide-y">
                  <div v-for="table in snapshot.tables" :key="table.name" class="grid grid-cols-[minmax(10rem,0.6fr)_minmax(10rem,1fr)_3rem] items-center gap-4 py-3 max-sm:grid-cols-1 max-sm:gap-2">
                    <div class="grid gap-0.5"><strong class="text-sm">{{ table.name }}</strong><span class="text-muted-foreground text-xs">{{ table.completed_chunks }}/{{ table.total_chunks }} chunks · {{ table.rows.toLocaleString() }} rows</span></div>
                    <Progress :model-value="snapshotPercent(table)" />
                    <strong class="font-mono text-sm">{{ snapshotPercent(table) }}%</strong>
                  </div>
                </div>
              </Card>
            </div>
          </TabsContent>

          <TabsContent value="replication">
            <div class="grid gap-4 md:grid-cols-2">
              <Card class="p-4">
                <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Checkpoint</p><h2 class="text-base font-semibold capitalize">{{ modeOf(selectedDatabase) }}</h2></div><Radio :size="20" class="text-muted-foreground" /></div>
                <dl class="grid grid-cols-2 gap-x-4">
                  <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">State</dt><dd class="mt-1 text-sm">{{ selectedDatabase.state }}</dd></div>
                  <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Poll cadence</dt><dd class="mt-1 text-sm">{{ selectedDatabase.poll_interval_seconds }}s</dd></div>
                  <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Reconcile</dt><dd class="mt-1 text-sm">{{ selectedDatabase.reconcile_interval_seconds }}s</dd></div>
                  <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Updated</dt><dd class="mt-1 text-sm">{{ formatDate(selectedDatabase.updated_at) }}</dd></div>
                </dl>
              </Card>
              <Card class="p-4">
                <div class="mb-4 flex items-center justify-between gap-3"><h2 class="text-base font-semibold">Dead-letter queue</h2><Badge :class="deadLetters.filter((item) => item.database_id === selectedDatabase?.id).length ? 'tone-negative' : 'tone-positive'">{{ deadLetters.filter((item) => item.database_id === selectedDatabase?.id).length }}</Badge></div>
                <div v-if="!deadLetters.filter((item) => item.database_id === selectedDatabase?.id).length" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 text-center"><Check :size="24" /><strong class="text-foreground">No rejected events</strong><span class="max-w-sm text-sm">Decoder and storage errors appear here.</span></div>
                <div v-for="record in deadLetters.filter((item) => item.database_id === selectedDatabase?.id)" :key="record.id" class="border-b py-3 last:border-0">
                  <strong class="text-sm">{{ record.table || 'database' }}</strong>
                  <p class="text-destructive mt-1 text-sm">{{ record.error }}</p>
                  <div class="mt-2 flex items-center gap-2">
                    <Button size="sm" :disabled="!record.table" @click="retryDlq(record)"><RefreshCw /> Retry safely</Button>
                    <Button variant="link" size="sm" @click="discardDlq(record)">Discard</Button>
                  </div>
                </div>
              </Card>
            </div>
          </TabsContent>

          <TabsContent value="schema">
            <Card class="p-4">
              <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Replica catalog</p><h2 class="text-base font-semibold">Schema generations</h2></div><Badge variant="outline">{{ tables.length }} tables</Badge></div>
              <div class="grid grid-cols-3 gap-2 max-sm:grid-cols-1">
                <button v-for="table in tables" :key="table.name" class="hover:border-foreground/30 hover:bg-accent grid grid-cols-[auto_1fr_auto] items-center gap-2.5 rounded-md border p-3 text-left" @click="describeTable(table)">
                  <Table2 :size="16" class="text-muted-foreground" /><span class="grid min-w-0"><strong class="truncate text-sm">{{ table.name }}</strong><small class="text-muted-foreground text-xs">Generation {{ table.schema_version }}</small></span><ChevronRight :size="15" class="text-muted-foreground" />
                </button>
              </div>
            </Card>
          </TabsContent>

          <TabsContent value="storage">
            <Card class="p-4">
              <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Columnar footprint</p><h2 class="text-base font-semibold">Storage posture</h2></div><HardDrive :size="20" class="text-muted-foreground" /></div>
              <div class="grid grid-cols-3 gap-3 max-sm:grid-cols-1">
                <div class="rounded-md border p-4"><span class="text-muted-foreground text-xs">Visible rows</span><strong class="mt-1 block text-2xl font-semibold tracking-tight">{{ formatNumber(statuses[selectedDatabase.id]?.rows || 0) }}</strong><small class="text-muted-foreground text-xs">Merge-on-read deduplicated</small></div>
                <div class="rounded-md border p-4"><span class="text-muted-foreground text-xs">Schema generations</span><strong class="mt-1 block text-2xl font-semibold tracking-tight">{{ tables.reduce((sum, table) => sum + table.schema_version, 0) }}</strong><small class="text-muted-foreground text-xs">Stable column IDs</small></div>
                <div class="rounded-md border p-4"><span class="text-muted-foreground text-xs">Compaction</span><strong class="mt-1 block text-2xl font-semibold tracking-tight">Auto</strong><small class="text-muted-foreground text-xs">Bounded size-tier passes</small></div>
              </div>
              <p class="text-muted-foreground mt-4 text-xs leading-relaxed">Exact segment bytes and compression ratios are exported by the operations metrics surface in M8.</p>
            </Card>
          </TabsContent>

          <TabsContent value="settings">
            <Card class="grid gap-4 p-4">
              <div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Replication controls</p><h2 class="text-base font-semibold">Database settings</h2></div>
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
              <dl class="grid grid-cols-2 gap-x-4">
                <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Poll cadence</dt><dd class="mt-1 text-sm">{{ selectedDatabase.poll_interval_seconds }} seconds</dd></div>
                <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Reconciliation</dt><dd class="mt-1 text-sm">{{ selectedDatabase.reconcile_interval_seconds }} seconds</dd></div>
                <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Included</dt><dd class="mt-1 text-sm">{{ selectedDatabase.include_tables.length || 'All tables' }}</dd></div>
                <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Excluded</dt><dd class="mt-1 text-sm">{{ selectedDatabase.exclude_tables.length || 'None' }}</dd></div>
              </dl>
            </Card>
          </TabsContent>
        </Tabs>
      </section>

      <section v-else-if="page === 'wizard'" class="mx-auto w-full max-w-4xl px-4 py-10 sm:px-6">
        <header class="mb-6"><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Add database</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Build a live mirror</h1><p class="text-muted-foreground mt-1.5">Connection, capability proof, table selection, then durable handoff.</p></header>
        <ol class="mb-4 grid grid-cols-4 gap-0">
          <li
            v-for="(label, index) in ['Connection', 'Probe', 'Tables', 'Start']"
            :key="label"
            class="relative flex items-center gap-2 text-xs after:absolute after:top-1/2 after:right-[0.6rem] after:left-8 after:h-px after:bg-border last:after:hidden"
            :class="wizard.step === index + 1 || wizard.step > index + 1 ? 'text-foreground font-semibold' : 'text-muted-foreground'"
          >
            <span
              class="z-10 grid size-6 shrink-0 place-items-center rounded-full border font-mono text-[0.6rem]"
              :class="wizard.step > index + 1 ? 'border-green text-green bg-green-soft' : wizard.step === index + 1 ? 'bg-foreground text-background border-foreground' : 'bg-background'"
            >{{ wizard.step > index + 1 ? '✓' : index + 1 }}</span>{{ label }}
          </li>
        </ol>
        <Card class="p-6 sm:p-8">
          <form v-if="wizard.step === 1" class="grid gap-6" @submit.prevent="wizardConnection">
            <div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">01 / Connection</p><h2 class="text-xl font-semibold">Where is MySQL?</h2><p class="text-muted-foreground mt-1.5">The DSN is encrypted before it enters the control-plane database.</p></div>
            <div class="grid gap-3 sm:grid-cols-2">
              <div class="grid content-start gap-1.5">
                <Label for="wizard-name">MySQL schema</Label>
                <Input id="wizard-name" v-model="wizard.name" required placeholder="analytics" />
                <small class="text-muted-foreground text-xs">Exact source schema name and case.</small>
              </div>
              <div class="grid content-start gap-1.5 sm:col-span-2">
                <Label for="wizard-dsn">MySQL DSN</Label>
                <Input id="wizard-dsn" v-model="wizard.dsn" required type="password" placeholder="mysql://pintail:secret@db.internal/analytics" />
              </div>
            </div>
            <p v-if="wizard.error" class="text-destructive text-sm">{{ wizard.error }}</p>
            <div class="flex justify-end gap-2">
              <Button type="button" variant="outline" @click="go('databases')">Cancel</Button>
              <Button type="submit" :disabled="wizard.working"><LoaderCircle v-if="wizard.working" class="animate-spin" /> Test connection <ArrowRight v-if="!wizard.working" /></Button>
            </div>
          </form>
          <div v-else-if="wizard.step === 2" class="grid gap-6">
            <div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">02 / Capability probe</p><h2 class="text-xl font-semibold">{{ wizard.serverVersion }}</h2><p class="text-muted-foreground mt-1.5">Pintail checks every invariant required for safe snapshot and stream ownership.</p></div>
            <div v-if="wizard.probe" class="grid grid-cols-2 rounded-md border max-sm:grid-cols-1">
              <div v-for="(value, key) in wizard.probe.capabilities" v-show="typeof value === 'boolean'" :key="key" class="grid min-h-14 grid-cols-[auto_1fr] items-center gap-2 border-b p-3 odd:border-r max-sm:odd:border-r-0">
                <span class="grid size-6 place-items-center rounded-full" :class="value ? 'bg-green-soft text-green' : 'bg-red-soft text-red'"><Check v-if="value" :size="14" /><X v-else :size="14" /></span>
                <span><strong class="block text-sm capitalize">{{ String(key).replaceAll('_', ' ') }}</strong><small class="text-muted-foreground text-xs">{{ value ? 'Pass' : 'Requires remediation' }}</small></span>
              </div>
            </div>
            <div class="border-foreground bg-accent flex gap-3 border-l-2 p-3.5">
              <Radio :size="18" class="shrink-0" />
              <div class="grid gap-1"><strong class="text-sm">Recommended: {{ wizard.probe?.capabilities.recommended_mode.toUpperCase() }}</strong><span class="text-muted-foreground text-xs">{{ wizard.probe?.capabilities.reasons.join(' · ') || 'All native replication requirements passed.' }}</span></div>
            </div>
            <div class="grid gap-2">
              <Label>Replication mode</Label>
              <RadioGroup v-model="wizard.mode" class="flex gap-5">
                <div class="flex items-center gap-2"><RadioGroupItem id="wizard-mode-cdc" value="cdc" /><Label for="wizard-mode-cdc">CDC</Label></div>
                <div class="flex items-center gap-2"><RadioGroupItem id="wizard-mode-polling" value="polling" /><Label for="wizard-mode-polling">Polling</Label></div>
              </RadioGroup>
            </div>
            <div class="flex justify-end gap-2"><Button variant="outline" @click="wizard.step = 1">Back</Button><Button @click="wizard.step = 3">Choose tables <ArrowRight /></Button></div>
          </div>
          <div v-else-if="wizard.step === 3 && wizard.probe" class="grid gap-6">
            <div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">03 / Table selection</p><h2 class="text-xl font-semibold">Choose the analytical surface</h2><p class="text-muted-foreground mt-1.5">PK-less append tables preserve rows but cannot model source updates or deletes.</p></div>
            <div class="grid rounded-md border">
              <div v-for="table in wizard.probe.tables" :key="table.name" class="grid grid-cols-[auto_1fr_auto_auto] items-center gap-3 border-b p-3 last:border-0">
                <Checkbox
                  :id="`wizard-pick-${table.name}`"
                  :model-value="wizard.includes.includes(table.name)"
                  @update:model-value="(checked) => toggleInclude(table.name, checked === true)"
                />
                <Label :for="`wizard-pick-${table.name}`" class="grid gap-0.5"><strong class="text-sm font-medium">{{ table.name }}</strong><small class="text-muted-foreground text-xs font-normal">{{ table.estimated_rows?.toLocaleString() || 'Unknown' }} rows · {{ table.engine || 'Unknown engine' }}</small></Label>
                <Badge :class="table.key.mode === 'append_row_id' ? 'tone-warning' : 'tone-positive'">{{ table.key.mode.replace('_', ' ') }}</Badge>
                <AlertTriangle v-if="table.warnings.length" :size="16" class="text-amber" />
              </div>
            </div>
            <p v-if="wizard.error" class="text-destructive text-sm">{{ wizard.error }}</p>
            <div class="flex justify-end gap-2"><Button variant="outline" @click="wizard.step = 2">Back</Button><Button :disabled="wizard.working || !wizard.includes.length" @click="finishWizard"><LoaderCircle v-if="wizard.working" class="animate-spin" /> Review & start <ArrowRight v-if="!wizard.working" /></Button></div>
          </div>
          <div v-else class="text-muted-foreground grid min-h-80 place-content-center justify-items-center gap-2 text-center"><LoaderCircle class="animate-spin" :size="28" /><h2 class="text-foreground font-semibold">Starting the mirror</h2><p class="max-w-sm text-sm">Capturing the source position and establishing resumable chunks.</p></div>
        </Card>
      </section>

      <section v-else-if="page === 'sql'" class="mx-auto flex w-full max-w-[88rem] flex-col px-4 py-10 sm:px-6">
        <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
          <div><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Native query engine</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">SQL Console</h1><p class="text-muted-foreground mt-1.5">MySQL dialect over reader-pinned columnar snapshots.</p></div>
          <Select v-model="sqlDatabaseId">
            <SelectTrigger class="min-w-52"><Database :size="15" /><SelectValue placeholder="Choose database" /></SelectTrigger>
            <SelectContent>
              <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
            </SelectContent>
          </Select>
        </header>
        <Card v-if="!databases.length" class="text-muted-foreground grid min-h-80 place-content-center justify-items-center gap-2 p-6 text-center"><SquareTerminal :size="30" /><h2 class="text-foreground font-semibold">No queryable mirror</h2><p class="max-w-md text-sm">Add and snapshot a database before opening the console.</p><Button @click="beginWizard">Add database</Button></Card>
        <template v-else>
          <Card class="overflow-hidden p-0">
            <div class="text-muted-foreground flex min-h-11 items-center justify-between border-b px-3 font-mono text-xs">
              <span>query.sql</span>
              <div class="flex items-center gap-3">
                <span class="bg-muted rounded border px-1.5 py-0.5 text-[0.58rem]">⌘ Enter</span>
                <Button size="sm" :disabled="sqlRunning" @click="runSql"><LoaderCircle v-if="sqlRunning" class="animate-spin" /><Play v-else /> Run</Button>
              </div>
            </div>
            <LazySqlEditor v-model="sqlText" @run="runSql" />
          </Card>
          <p v-if="sqlError" class="text-destructive my-3 text-sm">{{ sqlError }}</p>
          <Card class="mt-4 overflow-hidden p-0">
            <div class="flex flex-wrap items-center justify-between gap-3 border-b p-4">
              <div><h2 class="text-base font-semibold">Results</h2><p v-if="sqlResult" class="text-muted-foreground mt-1 font-mono text-xs">{{ sqlResult.stats.rows }} rows · {{ sqlResult.stats.duration_ms }} ms · {{ sqlResult.stats.blocks_read }} blocks read / {{ sqlResult.stats.blocks_pruned }} pruned</p></div>
              <div v-if="sqlResult" class="flex items-center gap-2">
                <Button variant="outline" size="sm" @click="exportResult('csv')">CSV</Button>
                <Button variant="outline" size="sm" @click="exportResult('json')">JSON</Button>
              </div>
            </div>
            <div v-if="!sqlResult" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 text-center"><Search :size="24" /><strong class="text-foreground">Run a query</strong><span class="max-w-sm text-sm">Typed fields and physical scan counters appear here.</span></div>
            <div v-else class="max-h-[34rem] overflow-auto">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead v-for="field in sqlResult.fields" :key="field.name" class="sticky top-0 z-10"><span>{{ field.name }}</span><small class="text-muted-foreground mt-0.5 block font-normal normal-case">{{ typeof field.data_type === 'string' ? field.data_type : 'typed' }}</small></TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  <TableRow v-for="(row, rowIndex) in sqlResult.rows" :key="rowIndex">
                    <TableCell v-for="(value, valueIndex) in row" :key="valueIndex" class="text-nowrap font-mono text-xs" :class="{ 'text-muted-foreground italic': value === null }">{{ displayValue(value) }}</TableCell>
                  </TableRow>
                </TableBody>
              </Table>
            </div>
          </Card>
        </template>
      </section>

      <section v-else-if="page === 'activity'" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
        <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
          <div><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Durable work log</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Activity</h1><p class="text-muted-foreground mt-1.5">Snapshot, stream, poll, and repair outcomes from control-plane records.</p></div>
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
        <Card class="overflow-hidden p-0">
          <div v-if="!filteredActivity.length" class="text-muted-foreground grid min-h-80 place-content-center justify-items-center gap-2 p-6 text-center"><Activity :size="28" /><h2 class="text-foreground font-semibold">No matching activity</h2><p class="max-w-md text-sm">Completed and failed replication work appears after the first snapshot.</p></div>
          <Table v-else>
            <TableHeader>
              <TableRow><TableHead>Started</TableHead><TableHead>Database</TableHead><TableHead>Kind</TableHead><TableHead>Status</TableHead><TableHead>Rows</TableHead><TableHead>Bytes</TableHead><TableHead>Duration</TableHead></TableRow>
            </TableHeader>
            <TableBody>
              <TableRow v-for="record in filteredActivity" :key="record.id">
                <TableCell class="text-muted-foreground">{{ formatDate(record.started_at) }}</TableCell>
                <TableCell><strong>{{ databases.find((item) => item.id === record.database_id)?.name || record.database_id }}</strong><small v-if="record.table" class="text-muted-foreground block text-xs">{{ record.table }}</small></TableCell>
                <TableCell class="capitalize">{{ record.kind }}</TableCell>
                <TableCell><Badge :class="`tone-${stateTone(record.status)}`">{{ record.status }}</Badge></TableCell>
                <TableCell class="font-mono">{{ record.rows.toLocaleString() }}</TableCell>
                <TableCell class="font-mono">{{ formatBytes(record.bytes) }}</TableCell>
                <TableCell class="font-mono">{{ record.duration_ms === null ? '—' : `${record.duration_ms} ms` }}</TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </Card>
        <Card v-if="deadLetters.length" class="mt-4 p-4">
          <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Requires judgment</p><h2 class="text-base font-semibold">Dead-letter queue</h2></div><Badge class="tone-negative">{{ deadLetters.length }}</Badge></div>
          <div class="grid gap-3 sm:grid-cols-2">
            <div v-for="record in deadLetters" :key="record.id" class="rounded-md border p-3">
              <div class="flex justify-between gap-3"><strong class="text-sm">{{ record.table || 'Database event' }}</strong><span class="text-muted-foreground text-xs">{{ formatDate(record.created_at) }}</span></div>
              <p class="text-destructive mt-1 text-sm">{{ record.error }}</p>
              <pre class="bg-muted text-muted-foreground mt-2 max-h-48 overflow-auto rounded p-2.5 text-xs">{{ JSON.stringify(record.event, null, 2) }}</pre>
              <div class="mt-2 flex items-center gap-2">
                <Button size="sm" :disabled="!record.table" @click="retryDlq(record)"><RefreshCw /> Retry safely</Button>
                <Button variant="destructive" size="sm" @click="discardDlq(record)">Discard</Button>
              </div>
            </div>
          </div>
        </Card>
      </section>

      <section v-else-if="page === 'keys'" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
        <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
          <div><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Database-scoped access</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">API Keys</h1><p class="text-muted-foreground mt-1.5">Secrets are SHA-256 hash-only and shown once.</p></div>
          <Select v-model="keyDatabaseId" @update:model-value="loadKeys">
            <SelectTrigger class="min-w-52"><Database :size="15" /><SelectValue placeholder="Choose database" /></SelectTrigger>
            <SelectContent>
              <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
            </SelectContent>
          </Select>
        </header>
        <Card class="mb-4 grid items-end gap-4 p-4 sm:grid-cols-[1.1fr_1fr_1fr_auto]">
          <div><h2 class="text-base font-semibold">Create a key</h2><p class="text-muted-foreground mt-1 text-sm">Use a narrow scope for each application.</p></div>
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
        </Card>
        <Alert v-if="revealedSecret" class="mb-4">
          <AlertTriangle />
          <AlertDescription class="flex w-full items-center gap-3">
            <div class="flex-1"><strong class="text-foreground block">Copy this secret now. It cannot be recovered.</strong><code class="mt-1 block break-all">{{ revealedSecret }}</code></div>
            <Button variant="ghost" size="icon-sm" class="shrink-0" @click="copy(revealedSecret)"><Copy /></Button>
            <Button variant="ghost" size="icon-sm" class="shrink-0" @click="revealedSecret = ''"><X /></Button>
          </AlertDescription>
        </Alert>
        <Card class="overflow-hidden p-0">
          <div v-if="!keys.length" class="text-muted-foreground grid min-h-80 place-content-center justify-items-center gap-2 p-6 text-center"><KeyRound :size="28" /><h2 class="text-foreground font-semibold">No keys for this database</h2><p class="max-w-md text-sm">Create one for the HTTP API or MySQL wire clients.</p></div>
          <Table v-else>
            <TableHeader>
              <TableRow><TableHead>Name</TableHead><TableHead>Scopes</TableHead><TableHead>Status</TableHead><TableHead>Last used</TableHead><TableHead>Created</TableHead><TableHead><span class="sr-only">Actions</span></TableHead></TableRow>
            </TableHeader>
            <TableBody>
              <TableRow v-for="key in keys" :key="key.id">
                <TableCell><strong>{{ key.name }}</strong><small class="text-muted-foreground block text-xs">{{ key.id }}</small></TableCell>
                <TableCell><Badge v-for="scope in key.scopes" :key="scope" variant="outline" class="mr-1">{{ scope }}</Badge></TableCell>
                <TableCell><Badge :class="key.enabled ? 'tone-positive' : 'tone-neutral'">{{ key.enabled ? 'enabled' : 'disabled' }}</Badge></TableCell>
                <TableCell class="text-muted-foreground">{{ formatDate(key.last_used_at) }}</TableCell>
                <TableCell class="text-muted-foreground">{{ formatDate(key.created_at) }}</TableCell>
                <TableCell>
                  <div class="flex items-center gap-1">
                    <Button variant="link" size="sm" @click="toggleKey(key)">{{ key.enabled ? 'Disable' : 'Enable' }}</Button>
                    <Button variant="ghost" size="icon-sm" @click="deleteKey(key)"><Trash2 /></Button>
                  </div>
                </TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </Card>
      </section>

      <section v-else-if="page === 'backups'" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
        <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
          <div><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Recovery plane</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Backups</h1><p class="text-muted-foreground mt-1.5">Checksum-verified manifests, immutable segments, and control-plane state restore side-by-side.</p></div>
          <Select v-model="backupDatabaseId" @update:model-value="() => loadBackups()">
            <SelectTrigger class="min-w-52"><Database :size="15" /><SelectValue placeholder="Choose database" /></SelectTrigger>
            <SelectContent>
              <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
            </SelectContent>
          </Select>
        </header>
        <div class="grid gap-4 md:grid-cols-2">
          <Card class="grid gap-4 p-4">
            <div class="flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">S3-compatible destination</p><h2 class="text-base font-semibold">Backup configuration</h2></div><Badge :class="backupConfigLoaded ? 'tone-positive' : 'tone-neutral'">{{ backupConfigLoaded ? 'Configured' : 'Not configured' }}</Badge></div>
            <div class="grid gap-3 sm:grid-cols-2">
              <div class="grid content-start gap-1.5"><Label for="backup-bucket">Bucket</Label><Input id="backup-bucket" v-model="backupForm.bucket" autocomplete="off" placeholder="analytics-backups" /></div>
              <div class="grid content-start gap-1.5"><Label for="backup-prefix">Object prefix</Label><Input id="backup-prefix" v-model="backupForm.prefix" autocomplete="off" placeholder="pintail/production" /></div>
              <div class="grid content-start gap-1.5 sm:col-span-2"><Label for="backup-endpoint">Endpoint <small class="text-muted-foreground font-normal">optional for AWS</small></Label><Input id="backup-endpoint" v-model="backupForm.endpoint" autocomplete="url" placeholder="http://minio.internal:9000" /></div>
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
            <div class="flex w-full items-center justify-between py-1">
              <span><strong class="block text-sm">Scheduled backups</strong><small class="text-muted-foreground text-xs">Runs after the next healthy supervised cycle when due.</small></span>
              <Switch :model-value="backupForm.enabled" @update:model-value="(value) => backupForm.enabled = value === true" />
            </div>
            <Button :disabled="backupLoading || !backupDatabaseId || !backupForm.bucket.trim() || !backupForm.prefix.trim()" @click="saveBackupConfig"><LoaderCircle v-if="backupLoading" class="animate-spin" /><HardDrive v-else /> Save destination</Button>
            <p class="text-muted-foreground text-xs leading-relaxed">Prefix validation prevents accidental broad writes; it is not a tenant-isolation boundary. Use bucket IAM for isolation.</p>
          </Card>
          <Card class="grid content-start gap-4 p-4">
            <div class="flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Manual recovery point</p><h2 class="text-base font-semibold">Backup now</h2></div><Archive :size="19" class="text-muted-foreground" /></div>
            <p class="text-muted-foreground text-sm">The first run is full. Later runs reuse unchanged immutable segment objects unless you force a new full chain.</p>
            <div class="flex items-center gap-2">
              <Button :disabled="backupLoading || !backupConfigLoaded" @click="runBackup(false)"><Play /> Backup now</Button>
              <Button variant="outline" :disabled="backupLoading || !backupConfigLoaded" @click="runBackup(true)"><RefreshCw /> Force full</Button>
            </div>
            <Separator />
            <div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Side-by-side restore</p><h2 class="text-base font-semibold">Restore as new database</h2></div>
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
            <p class="text-muted-foreground text-xs leading-relaxed">Restore never overwrites a live mirror. The new database is detached from ingestion until new source credentials are supplied.</p>
          </Card>
        </div>
        <Card class="mt-4 overflow-hidden p-0">
          <div class="flex items-center justify-between gap-3 p-4 pb-0"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Durable audit</p><h2 class="text-base font-semibold">Backup history</h2></div><Button variant="ghost" size="icon" :disabled="backupLoading" aria-label="Refresh backup history" @click="loadBackups()"><RefreshCw /></Button></div>
          <div v-if="!backups.length" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 p-6 text-center"><Archive :size="26" /><strong class="text-foreground">No backup artifacts</strong><span class="max-w-sm text-sm">Save a destination, then create the first full recovery point.</span></div>
          <Table v-else>
            <TableHeader>
              <TableRow><TableHead>Started</TableHead><TableHead>Kind</TableHead><TableHead>Status</TableHead><TableHead>Objects</TableHead><TableHead>Uploaded</TableHead><TableHead>Chain</TableHead><TableHead>Error</TableHead></TableRow>
            </TableHeader>
            <TableBody>
              <TableRow v-for="backup in backups" :key="backup.id">
                <TableCell><strong>{{ formatDate(backup.started_at) }}</strong><small class="text-muted-foreground block font-mono text-xs">{{ backup.id }}</small></TableCell>
                <TableCell><Badge :class="backup.kind === 'full' ? 'tone-positive' : 'tone-neutral'">{{ backup.kind }}</Badge></TableCell>
                <TableCell><Badge :class="`tone-${stateTone(backup.status)}`">{{ backup.status }}</Badge></TableCell>
                <TableCell class="font-mono">{{ backup.object_count }}</TableCell>
                <TableCell class="font-mono">{{ formatBytes(backup.bytes) }}</TableCell>
                <TableCell><span v-if="backup.parent_id" class="font-mono">{{ backup.parent_id }}</span><span v-else class="text-muted-foreground">root</span></TableCell>
                <TableCell class="text-destructive max-w-72 text-xs">{{ backup.error || '—' }}</TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </Card>
      </section>

      <section v-else-if="page === 'settings'" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
        <header class="mb-7"><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Node policy</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Settings</h1><p class="text-muted-foreground mt-1.5">Operator identity, network surfaces, and local presentation.</p></header>
        <div class="grid items-start gap-4 sm:grid-cols-2">
          <Card class="p-4">
            <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Operator</p><h2 class="text-base font-semibold">Current session</h2></div><Server :size="19" class="text-muted-foreground" /></div>
            <dl class="grid grid-cols-2 gap-x-4">
              <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Subject</dt><dd class="mt-1 font-mono text-sm">{{ session.subject }}</dd></div>
              <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Role</dt><dd class="mt-1 text-sm">{{ session.role }}</dd></div>
              <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Scopes</dt><dd class="mt-1 text-sm">{{ session.scopes.join(', ') }}</dd></div>
              <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Session</dt><dd class="mt-1 text-sm">12-hour signed JWT</dd></div>
            </dl>
          </Card>
          <Card class="p-4">
            <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Appearance</p><h2 class="text-base font-semibold">Interface</h2></div><Button variant="ghost" size="icon" @click="toggleTheme"><Sun v-if="dark" /><Moon v-else /></Button></div>
            <div class="flex w-full items-center justify-between py-1">
              <span><strong class="block text-sm">Dark instrument panel</strong><small class="text-muted-foreground text-xs">Stored only in this browser.</small></span>
              <Switch :model-value="dark" @update:model-value="() => toggleTheme()" />
            </div>
          </Card>
          <Card class="overflow-hidden p-4">
            <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">MySQL wire</p><h2 class="text-base font-semibold">Client endpoint</h2></div><Badge :class="nodeStatus?.wire.enabled ? 'tone-positive' : 'tone-negative'">{{ nodeStatus?.wire.enabled ? 'Live' : 'Unavailable' }}</Badge></div>
            <div class="bg-muted mb-3 flex items-center gap-2.5 rounded-md border p-3">
              <span class="size-2 shrink-0 rounded-full" :class="nodeStatus?.wire.enabled ? 'bg-green' : 'bg-destructive'" />
              <code class="truncate text-sm">{{ nodeStatus?.wire.bind || 'Endpoint unavailable' }}</code>
            </div>
            <dl class="grid grid-cols-2 gap-x-4">
              <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Mode</dt><dd class="mt-1 text-sm">Read-only</dd></div>
              <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Authentication</dt><dd class="mt-1 text-sm">Database API key</dd></div>
              <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Username</dt><dd class="mt-1 text-sm">Database name</dd></div>
              <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Protocol</dt><dd class="mt-1 text-sm">MySQL native</dd></div>
            </dl>
          </Card>
          <Card class="p-4">
            <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Telemetry</p><h2 class="text-base font-semibold">Operations</h2></div><Badge class="tone-positive">Live</Badge></div>
            <dl class="grid grid-cols-2 gap-x-4">
              <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Metrics</dt><dd class="mt-1 text-sm"><a href="/metrics" target="_blank" class="underline underline-offset-2">/metrics</a></dd></div>
              <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Format</dt><dd class="mt-1 text-sm">Prometheus text</dd></div>
              <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Supervisor</dt><dd class="mt-1 text-sm">Isolated per database</dd></div>
              <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Recovery</dt><dd class="mt-1 text-sm">Scheduled + manual</dd></div>
            </dl>
          </Card>
        </div>
      </section>

      <section v-else-if="page === 'connect'" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
        <header class="mb-7"><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Client handoff</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Connect to Pintail</h1><p class="text-muted-foreground mt-1.5">The database name is the username; its scoped API key is the password.</p></header>
        <form class="bg-card text-card-foreground ring-foreground/10 mb-4 grid gap-4 rounded-lg p-4 ring-1 sm:grid-cols-4" @submit.prevent>
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
        <Card class="mb-4 grid grid-cols-[auto_1fr_auto] items-center gap-3 p-4 max-sm:grid-cols-[auto_1fr]">
          <Radio :size="17" class="text-muted-foreground" />
          <div class="grid gap-0.5"><strong class="text-sm">Native challenge, no stored plaintext.</strong><span class="text-muted-foreground text-xs">Use MySQL 8.4, mysql2, PyMySQL, DBeaver, or Metabase. Oracle's MySQL 9.x CLI removed its native-password client plugin.</span></div>
          <Button variant="link" size="sm" class="max-sm:col-span-2 max-sm:justify-self-start" @click="go('keys')">Create or rotate key <ArrowRight /></Button>
        </Card>
        <div class="grid gap-4 sm:grid-cols-2">
          <Card v-for="kind in (['mysql', 'node', 'python'] as const)" :key="kind" class="overflow-hidden p-0">
            <div class="flex items-center justify-between gap-3 p-4 pb-3"><h2 class="text-base font-semibold">{{ kind === 'mysql' ? 'MySQL CLI' : kind === 'node' ? 'Node.js' : 'Python' }}</h2><Button variant="ghost" size="icon" @click="copy(connectSnippet(kind))"><Copy /></Button></div>
            <pre class="bg-muted overflow-auto p-3.5 text-xs leading-relaxed break-all whitespace-pre-wrap">{{ connectSnippet(kind) }}</pre>
          </Card>
          <Card class="p-4">
            <div class="mb-4 flex items-center justify-between gap-3"><h2 class="text-base font-semibold">DBeaver / Metabase</h2><CircleHelp :size="17" class="text-muted-foreground" /></div>
            <dl class="mb-4 grid grid-cols-2 gap-x-4">
              <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Driver</dt><dd class="mt-1 text-sm">MySQL 8</dd></div>
              <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Host / port</dt><dd class="mt-1 text-sm">{{ connectHost }}:{{ connectPort }}</dd></div>
              <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Database / user</dt><dd class="mt-1 text-sm">{{ selectedConnectDatabase?.name || 'analytics' }}</dd></div>
              <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Password</dt><dd class="mt-1 text-sm">Query-scoped API key</dd></div>
            </dl>
            <p class="text-muted-foreground text-xs leading-relaxed">Keep SSL disabled for a loopback endpoint. Terminate TLS at your private ingress when clients connect across a network.</p>
          </Card>
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
