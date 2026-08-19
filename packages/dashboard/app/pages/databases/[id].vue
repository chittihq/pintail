<script setup lang="ts">
import { AlertTriangle, Check, ChevronRight, Eye, HardDrive, LoaderCircle, Pause, Play, Radio, RefreshCw, Table2, X } from '@lucide/vue'
import { useIntervalFn } from '@vueuse/core'
import { displayValue, formatDate, formatNumber, messageOf, modeOf, snapshotPercent, stateTone } from '@/lib/format'
import type { DlqRecord, QueryResponse, SnapshotStatus, TableSummary } from '@/types/pintail'

const route = useRoute()
const router = useRouter()
const { request } = usePintailApi()
const { databases, statuses, deadLetters, error, loading, setMode, setReconcileInterval, forceSnapshot, resetDatabase, runTableAction, discardDlq, retryDlq, tableProgress, seedTableProgress } = useControlPlane()

const databaseId = computed(() => String(route.params.id))
const database = computed(() => databases.value.find((item) => item.id === databaseId.value) ?? null)
const detailTab = computed({
  get: () => (typeof route.query.tab === 'string' ? route.query.tab : 'tables'),
  set: (value) => router.replace({ query: { ...route.query, tab: value } }),
})
const tableAction = ref('')
const tables = ref<TableSummary[]>([])
const snapshot = ref<SnapshotStatus | null>(null)

async function loadDatabaseDetail(showLoading = true) {
  if (showLoading) loading.value = true
  try {
    const [tableRows, snapshotStatus] = await Promise.all([
      request<TableSummary[]>(`/tables?db=${encodeURIComponent(databaseId.value)}`),
      request<SnapshotStatus>(`/databases/${encodeURIComponent(databaseId.value)}/snapshot/status`),
    ])
    tables.value = tableRows
    snapshot.value = snapshotStatus
    // A copy that was already running when this page loaded gets its bar
    // back from the server's retained progress instead of an empty badge.
    seedTableProgress(databaseId.value, tableRows)
    // The banner is shared and sticky - nothing else on this page clears it -
    // so a single failed poll left "database does not exist" on screen for
    // the rest of the session while the page beneath it loaded perfectly.
    // A load that succeeds is the evidence the last failure is over.
    error.value = ''
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    if (showLoading) loading.value = false
  }
}

watch(databaseId, () => loadDatabaseDetail(), { immediate: true })
useIntervalFn(() => void loadDatabaseDetail(false), 8_000)

async function pauseResume() {
  if (!database.value) return
  await setMode(database.value, database.value.mode === 'paused' ? 'auto' : 'paused')
}

/// Reconcile interval, in seconds, as edited.
///
/// Seeded from the record and reset whenever it reloads, so an operator never
/// edits a value that has since changed underneath them.
const reconcileDraft = ref<number | null>(null)
const savingReconcile = ref(false)
watch(
  () => database.value?.reconcile_interval_seconds,
  (seconds) => {
    if (seconds !== undefined) reconcileDraft.value = seconds
  },
  { immediate: true },
)

async function saveReconcileInterval() {
  if (!database.value || reconcileDraft.value === null) return
  savingReconcile.value = true
  try {
    await setReconcileInterval(database.value, reconcileDraft.value)
    await loadDatabaseDetail(false)
  } finally {
    savingReconcile.value = false
  }
}

const resnapshotting = ref(false)

async function resnapshot() {
  if (!database.value || resnapshotting.value) return
  resnapshotting.value = true
  try {
    // Navigating to the snapshot tab is the "it started" signal, so it only
    // happens when the request actually succeeded - a failure used to land
    // there anyway, showing the OLD journal as if a new run were underway.
    if (!(await forceSnapshot(database.value.id))) return
    detailTab.value = 'snapshot'
    await loadDatabaseDetail()
  } finally {
    resnapshotting.value = false
  }
}

const viewTable = ref<TableSummary | null>(null)
const viewResult = ref<QueryResponse | null>(null)
const viewError = ref('')

async function openView(table: TableSummary) {
  viewTable.value = table
  viewResult.value = null
  viewError.value = ''
  try {
    // The wire quoting rule: a backtick inside an identifier doubles.
    const identifier = table.name.replaceAll('`', '``')
    viewResult.value = await request<QueryResponse>('/query', {
      method: 'POST',
      body: JSON.stringify({
        db: databaseId.value,
        sql: `SELECT * FROM \`${identifier}\` LIMIT 100`,
      }),
    })
  } catch (failure) {
    viewError.value = messageOf(failure)
  }
}

function closeView(open: boolean) {
  if (!open) {
    viewTable.value = null
    viewResult.value = null
    viewError.value = ''
  }
}

const resetOpen = ref(false)
const resetting = ref(false)

async function confirmReset() {
  if (!database.value || resetting.value) return
  resetting.value = true
  try {
    if (!(await resetDatabase(database.value.id))) return
    resetOpen.value = false
    detailTab.value = 'snapshot'
    await loadDatabaseDetail()
  } finally {
    resetting.value = false
  }
}

const discardCandidate = ref<DlqRecord | null>(null)
const discarding = ref(false)

async function confirmDiscard() {
  if (!discardCandidate.value || discarding.value) return
  discarding.value = true
  try {
    await discardDlq(discardCandidate.value)
    discardCandidate.value = null
  } finally {
    discarding.value = false
  }
}

async function onTableAction(table: TableSummary, action: 'resync' | 'reconcile') {
  tableAction.value = `${table.name}:${action}`
  try {
    await runTableAction(databaseId.value, table, action)
    await loadDatabaseDetail()
  } catch {
    // error already recorded by runTableAction
  } finally {
    tableAction.value = ''
  }
}

/// Live copy progress for a table, when the event stream has reported any.
/// The bar's fraction derives from elapsed/(elapsed + eta): the snapshot
/// engine computes the ETA from measured throughput, so the fraction moves
/// honestly even though the source's total row count is unknown here.
function progressOf(table: TableSummary) {
  const entry = tableProgress.value[`${databaseId.value}:${table.name}`]
  if (!entry || table.state !== 'snapshotting') return null
  const elapsed = (Date.now() - entry.startedAt) / 1000
  const fraction = entry.etaSeconds != null && entry.etaSeconds >= 0
    ? Math.min(0.99, elapsed / Math.max(elapsed + entry.etaSeconds, 1))
    : null
  return {
    rows: entry.rows,
    fraction,
    eta: entry.etaSeconds != null && entry.etaSeconds >= 0 ? Math.round(entry.etaSeconds) : null,
  }
}

function describeTable(table: TableSummary) {
  navigateTo(`/sql?db=${databaseId.value}&describe=${encodeURIComponent(table.name)}`)
}
</script>

<template>
  <section v-if="database" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
      <div>
        <Button variant="link" size="sm" class="mb-2 px-0" as-child><NuxtLink to="/databases">Databases /</NuxtLink></Button>
        <h1 class="text-2xl font-bold tracking-tight sm:text-3xl">{{ database.name }}</h1>
        <div class="text-muted-foreground mt-3 flex items-center gap-2 text-sm">
          <Badge :class="`tone-${stateTone(database.state)}`">{{ database.state }}</Badge>
          <Badge variant="outline">{{ modeOf(database) }}</Badge>
          <span>{{ statuses[database.id]?.rows.toLocaleString() || 0 }} visible rows</span>
        </div>
      </div>
      <div class="flex items-center gap-2">
        <Button variant="outline" @click="pauseResume">
          <Play v-if="database.mode === 'paused'" /><Pause v-else />
          {{ database.mode === 'paused' ? 'Resume' : 'Pause' }}
        </Button>
        <Button :disabled="resnapshotting" @click="resnapshot"><LoaderCircle v-if="resnapshotting" class="animate-spin" /><RefreshCw v-else /> Resnapshot</Button>
      </div>
    </header>

    <Alert v-if="modeOf(database) === 'polling'" variant="destructive" class="mb-4">
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
                    title="A source foreign key performs ON DELETE/UPDATE CASCADE or SET NULL into this table. MySQL applies both inside InnoDB without writing row events, so they cannot reach the replica through CDC; these rows converge on the reconcile interval rather than in seconds."
                  >fk repair</Badge>
                  <Badge
                    v-if="table.key_mode === 'append_row_id'"
                    class="tone-warning ml-1.5"
                    :title="table.remediation === 'resnapshot' ? 'An ambiguous source mutation was quarantined. Resnapshot to restore exact duplicate multiplicity.' : 'Inserts replicate exactly; an UPDATE or DELETE is quarantined instead of choosing an arbitrary duplicate and requires resnapshot.'"
                  >keyless · {{ table.mutation_guarantee.replaceAll('_', ' ') }}</Badge>
                </TableCell>
                <TableCell>
                  <Badge :class="`tone-${stateTone(table.state)}`">{{ table.state }}</Badge>
                  <div v-if="progressOf(table)" data-testid="resnapshot-progress" class="mt-1.5 w-40">
                    <div class="bg-muted h-1.5 w-full overflow-hidden rounded-full">
                      <div
                        class="bg-primary h-full rounded-full transition-all duration-500"
                        :class="progressOf(table)!.fraction == null ? 'w-1/3 animate-pulse' : ''"
                        :style="progressOf(table)!.fraction != null ? { width: `${Math.round(progressOf(table)!.fraction! * 100)}%` } : {}"
                      />
                    </div>
                    <span class="text-muted-foreground mt-0.5 block text-xs tabular-nums">
                      {{ progressOf(table)!.rows.toLocaleString() }} rows copied<template v-if="progressOf(table)!.eta != null"> · ~{{ progressOf(table)!.eta }}s left</template>
                    </span>
                  </div>
                </TableCell>
                <TableCell class="font-mono">{{ table.rows.toLocaleString() }}</TableCell>
                <TableCell class="font-mono">v{{ table.schema_version }}</TableCell>
                <TableCell class="text-muted-foreground"><span class="block max-w-72 truncate" :title="table.last_error || undefined">{{ table.last_error || '—' }}</span></TableCell>
                <TableCell>
                  <div class="flex items-center gap-1">
                    <Button variant="link" size="sm" title="First 100 rows as the query engine serves them" @click="openView(table)">
                      <Eye /> View
                    </Button>
                    <Button variant="link" size="sm" :disabled="Boolean(tableAction)" @click="onTableAction(table, 'reconcile')">
                      <LoaderCircle v-if="tableAction === `${table.name}:reconcile`" class="animate-spin" /> Reconcile
                    </Button>
                    <Button variant="link" size="sm" :disabled="Boolean(tableAction)" title="Recopies only this table from the source, behind its own binlog fence" @click="onTableAction(table, 'resync')">
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
            <div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Durable publication</p><h2 class="text-base font-semibold capitalize">{{ snapshot?.state || database.state }}</h2><p class="text-muted-foreground mt-1.5 text-sm">Progress advances only after a chunk and its control-plane checkpoint are durable.</p></div>
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
            <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Checkpoint</p><h2 class="text-base font-semibold capitalize">{{ modeOf(database) }}</h2></div><Radio :size="20" class="text-muted-foreground" /></div>
            <dl class="grid grid-cols-2 gap-x-4">
              <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">State</dt><dd class="mt-1 text-sm">{{ database.state }}</dd></div>
              <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Poll cadence</dt><dd class="mt-1 text-sm">{{ database.poll_interval_seconds }}s</dd></div>
              <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Reconcile</dt><dd class="mt-1 text-sm">{{ database.reconcile_interval_seconds }}s</dd></div>
              <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Updated</dt><dd class="mt-1 text-sm">{{ formatDate(database.updated_at) }}</dd></div>
            </dl>
          </Card>
          <Card class="p-4">
            <div class="mb-4 flex items-center justify-between gap-3"><h2 class="text-base font-semibold">Dead-letter queue</h2><Badge :class="deadLetters.filter((item) => item.database_id === database?.id).length ? 'tone-negative' : 'tone-positive'">{{ deadLetters.filter((item) => item.database_id === database?.id).length }}</Badge></div>
            <div v-if="!deadLetters.filter((item) => item.database_id === database?.id).length" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 text-center"><Check :size="24" /><strong class="text-foreground">No rejected events</strong><span class="max-w-sm text-sm">Decoder and storage errors appear here.</span></div>
            <div v-for="record in deadLetters.filter((item) => item.database_id === database?.id)" :key="record.id" class="border-b py-3 last:border-0">
              <strong class="text-sm">{{ record.table || 'database' }}</strong>
              <p class="text-destructive mt-1 text-sm break-words">{{ record.error }}</p>
              <div class="mt-2 flex items-center gap-2">
                <Button size="sm" :disabled="!record.table" @click="retryDlq(record)"><RefreshCw /> Retry safely</Button>
                <Button variant="link" size="sm" @click="discardCandidate = record">Discard</Button>
              </div>
            </div>
          </Card>
        </div>
      </TabsContent>

      <TabsContent value="schema">
        <Card class="p-4">
          <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Replica catalog</p><h2 class="text-base font-semibold">Schema generations</h2></div><Badge variant="outline">{{ tables.length }} tables</Badge></div>
          <div class="grid grid-cols-3 gap-2 max-sm:grid-cols-1">
            <button v-for="table in tables" :key="table.name" class="hover:border-foreground/30 hover:bg-accent grid grid-cols-[auto_1fr_auto] items-center gap-2.5 rounded-md border p-3 text-left" @click="describeTable(table)">
              <Table2 :size="16" class="text-muted-foreground" /><span class="grid min-w-0"><strong class="truncate text-sm">{{ table.name }}</strong><small class="text-muted-foreground text-xs">Generation {{ table.schema_version }}</small></span><ChevronRight :size="15" class="text-muted-foreground" />
            </button>
          </div>
        </Card>
      </TabsContent>

      <TabsContent value="storage">
        <Card class="p-4">
          <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Columnar footprint</p><h2 class="text-base font-semibold">Storage posture</h2></div><HardDrive :size="20" class="text-muted-foreground" /></div>
          <div class="grid grid-cols-3 gap-3 max-sm:grid-cols-1">
            <div class="rounded-md border p-4"><span class="text-muted-foreground text-xs">Visible rows</span><strong class="mt-1 block text-2xl font-semibold tracking-tight">{{ formatNumber(statuses[database.id]?.rows || 0) }}</strong><small class="text-muted-foreground text-xs">Merge-on-read deduplicated</small></div>
            <div class="rounded-md border p-4"><span class="text-muted-foreground text-xs">Schema generations</span><strong class="mt-1 block text-2xl font-semibold tracking-tight">{{ tables.reduce((sum, table) => sum + table.schema_version, 0) }}</strong><small class="text-muted-foreground text-xs">Stable column IDs</small></div>
            <div class="rounded-md border p-4"><span class="text-muted-foreground text-xs">Compaction</span><strong class="mt-1 block text-2xl font-semibold tracking-tight">Auto</strong><small class="text-muted-foreground text-xs">Bounded size-tier passes</small></div>
          </div>
          <p class="text-muted-foreground mt-4 text-xs leading-relaxed">Exact segment bytes and compression ratios are exported by the operations metrics surface in M8.</p>
        </Card>
      </TabsContent>

      <TabsContent value="settings">
        <Card class="grid gap-4 p-4">
          <div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Replication controls</p><h2 class="text-base font-semibold">Database settings</h2></div>
          <div class="grid max-w-xs gap-1.5">
            <Label>Requested mode</Label>
            <Select :model-value="database.mode" @update:model-value="(value) => setMode(database!, value as typeof database.mode)">
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
            <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Poll cadence</dt><dd class="mt-1 text-sm">{{ database.poll_interval_seconds }} seconds</dd></div>
            <div class="border-b py-3">
              <dt class="text-muted-foreground font-mono text-xs uppercase">Reconciliation</dt>
              <dd class="mt-1 flex items-center gap-2">
                <input
                  v-model.number="reconcileDraft"
                  type="number"
                  min="10"
                  step="10"
                  class="border-input bg-background w-24 rounded-md border px-2 py-1 text-sm"
                  aria-label="Reconcile interval in seconds"
                >
                <span class="text-muted-foreground text-sm">seconds</span>
                <Button
                  size="sm"
                  variant="outline"
                  :disabled="savingReconcile || reconcileDraft === database.reconcile_interval_seconds"
                  title="Rows removed by a foreign-key cascade or SET NULL never arrive through CDC; they converge on this interval."
                  @click="saveReconcileInterval"
                >Save</Button>
              </dd>
            </div>
            <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Included</dt><dd class="mt-1 text-sm">{{ database.include_tables.length || 'All tables' }}</dd></div>
            <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Excluded</dt><dd class="mt-1 text-sm">{{ database.exclude_tables.length || 'None' }}</dd></div>
          </dl>
        </Card>
        <Card class="mt-4 grid gap-3 p-4">
          <div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Recovery of last resort</p><h2 class="text-base font-semibold">Start the mirror over</h2></div>
          <p class="text-muted-foreground text-sm">
            Clears every mirrored table, checkpoint and quarantined event, re-probes the source
            with the saved connection, and copies everything again. Replication then continues in
            the mode configured above. The connection, API keys and backups are untouched.
          </p>
          <div><Button variant="destructive" :disabled="resetting" data-testid="reset-mirror" @click="resetOpen = true"><LoaderCircle v-if="resetting" class="animate-spin" /> Reset mirror</Button></div>
        </Card>
      </TabsContent>
    </Tabs>
  </section>
  <section v-else class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <Skeleton v-if="loading" class="h-96" />
    <div v-else class="text-muted-foreground grid min-h-80 place-content-center justify-items-center gap-2 text-center">
      <strong class="text-foreground">Database not found</strong>
      <Button variant="link" as-child><NuxtLink to="/databases">Back to databases</NuxtLink></Button>
    </div>
  </section>
  <Dialog :open="Boolean(viewTable)" @update:open="closeView">
    <DialogContent class="sm:max-w-5xl">
      <DialogHeader>
        <DialogTitle class="font-mono">{{ viewTable?.name }}</DialogTitle>
        <DialogDescription>
          <template v-if="viewResult">
            First {{ viewResult.rows.length }} rows as the query engine serves them ·
            {{ viewResult.stats.duration_ms }}ms
          </template>
          <template v-else-if="viewError">The query engine refused the read.</template>
          <template v-else>Reading…</template>
        </DialogDescription>
      </DialogHeader>
      <p v-if="viewError" class="text-destructive text-sm break-words">{{ viewError }}</p>
      <div v-else-if="!viewResult" class="grid min-h-40 place-content-center"><LoaderCircle class="animate-spin" /></div>
      <div v-else-if="!viewResult.rows.length" class="text-muted-foreground grid min-h-40 place-content-center text-sm">The table is empty.</div>
      <div v-else class="max-h-[60vh] overflow-auto rounded-md border">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead v-for="field in viewResult.fields" :key="field.name" class="bg-background sticky top-0 z-10"><span>{{ field.name }}</span><small class="text-muted-foreground mt-0.5 block font-normal normal-case">{{ typeof field.data_type === 'string' ? field.data_type : 'typed' }}</small></TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            <TableRow v-for="(row, rowIndex) in viewResult.rows" :key="rowIndex">
              <TableCell v-for="(value, valueIndex) in row" :key="valueIndex" class="text-nowrap font-mono text-xs" :class="{ 'text-muted-foreground italic': value === null }">{{ displayValue(value) }}</TableCell>
            </TableRow>
          </TableBody>
        </Table>
      </div>
      <DialogFooter>
        <Button variant="outline" as-child><NuxtLink :to="`/sql?db=${databaseId}&describe=${encodeURIComponent(viewTable?.name ?? '')}`">Open in SQL Console</NuxtLink></Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>

  <ConfirmActionDialog
    :open="resetOpen"
    :title="`Reset the ${database?.name} mirror?`"
    description="All mirrored data for this database is deleted and copied again from the source. Queries answer against partial data until the snapshot completes. The saved connection, mode, API keys and backups are kept."
    confirm-label="Reset mirror"
    :working="resetting"
    @confirm="confirmReset"
    @update:open="(open) => { resetOpen = open }"
  />

  <ConfirmActionDialog
    :open="Boolean(discardCandidate)"
    :title="`Discard this ${discardCandidate?.table || 'database'} dead letter?`"
    description="The quarantined event is deleted permanently and its change is never applied to the mirror. Retry it instead if the failure may have been transient."
    confirm-label="Discard"
    :working="discarding"
    @confirm="confirmDiscard"
    @update:open="(open) => { if (!open) discardCandidate = null }"
  />
</template>
