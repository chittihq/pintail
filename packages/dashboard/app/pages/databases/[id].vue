<script setup lang="ts">
import { AlertTriangle, Check, ChevronRight, HardDrive, LoaderCircle, Pause, Play, Radio, RefreshCw, Table2, X } from '@lucide/vue'
import { useIntervalFn } from '@vueuse/core'
import { formatDate, formatNumber, messageOf, modeOf, snapshotPercent, stateTone } from '@/lib/format'
import type { SnapshotStatus, TableSummary } from '@/types/pintail'

const route = useRoute()
const router = useRouter()
const { request } = usePintailApi()
const { databases, statuses, deadLetters, error, loading, setMode, forceSnapshot, runTableAction, discardDlq, retryDlq } = useControlPlane()

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

async function resnapshot() {
  if (!database.value) return
  await forceSnapshot(database.value.id)
  detailTab.value = 'snapshot'
  await loadDatabaseDetail()
}

async function onTableAction(table: TableSummary, action: 'resync' | 'reconcile') {
  tableAction.value = `${table.name}:${action}`
  try {
    await runTableAction(databaseId.value, table, action)
    if (action === 'resync') detailTab.value = 'snapshot'
    await loadDatabaseDetail()
  } catch {
    // error already recorded by runTableAction
  } finally {
    tableAction.value = ''
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
        <Button @click="resnapshot"><RefreshCw /> Resnapshot</Button>
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
                    title="A source foreign key cascades into this table. MySQL performs cascades inside InnoDB without writing row events, so they cannot reach the replica through CDC; these rows converge on the reconcile interval rather than in seconds."
                  >cascade</Badge>
                  <Badge
                    v-if="table.key_mode === 'append_row_id'"
                    class="tone-warning ml-1.5"
                    :title="table.remediation === 'resnapshot' ? 'An ambiguous source mutation was quarantined. Resnapshot to restore exact duplicate multiplicity.' : 'Inserts replicate exactly; an UPDATE or DELETE is quarantined instead of choosing an arbitrary duplicate and requires resnapshot.'"
                  >keyless · {{ table.mutation_guarantee.replaceAll('_', ' ') }}</Badge>
                </TableCell>
                <TableCell><Badge :class="`tone-${stateTone(table.state)}`">{{ table.state }}</Badge></TableCell>
                <TableCell class="font-mono">{{ table.rows.toLocaleString() }}</TableCell>
                <TableCell class="font-mono">v{{ table.schema_version }}</TableCell>
                <TableCell class="text-muted-foreground">{{ table.last_error || '—' }}</TableCell>
                <TableCell>
                  <div class="flex items-center gap-1">
                    <Button variant="link" size="sm" :disabled="Boolean(tableAction)" @click="onTableAction(table, 'reconcile')">
                      <LoaderCircle v-if="tableAction === `${table.name}:reconcile`" class="animate-spin" /> Reconcile
                    </Button>
                    <Button variant="link" size="sm" :disabled="Boolean(tableAction)" title="Starts a mirror-wide resnapshot because all tables share one source checkpoint" @click="onTableAction(table, 'resync')">
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
            <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Reconciliation</dt><dd class="mt-1 text-sm">{{ database.reconcile_interval_seconds }} seconds</dd></div>
            <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Included</dt><dd class="mt-1 text-sm">{{ database.include_tables.length || 'All tables' }}</dd></div>
            <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Excluded</dt><dd class="mt-1 text-sm">{{ database.exclude_tables.length || 'None' }}</dd></div>
          </dl>
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
</template>
