<script setup lang="ts">
import { Activity as ActivityIcon, RefreshCw, ShieldCheck } from '@lucide/vue'
import { formatBytes, formatDate, messageOf, stateTone } from '@/lib/format'
import type { AuditEvent } from '@/types/pintail'

const route = useRoute()
const router = useRouter()
const { databases, activity, deadLetters, discardDlq, retryDlq, session, error } = useControlPlane()
const { request } = usePintailApi()

const activityDatabase = computed({
  get: () => (typeof route.query.db === 'string' ? route.query.db : ''),
  set: (value) => router.replace({ query: { ...route.query, db: value || undefined } }),
})
const filteredActivity = computed(() =>
  activityDatabase.value
    ? activity.value.filter((record) => record.database_id === activityDatabase.value)
    : activity.value,
)

const isAdmin = computed(() => session.value?.role === 'admin')
const auditEvents = ref<AuditEvent[]>([])
const auditLoading = ref(false)

async function loadAuditLog() {
  if (!isAdmin.value) return
  auditLoading.value = true
  try {
    auditEvents.value = await request<AuditEvent[]>('/workspaces/audit-log?limit=200')
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    auditLoading.value = false
  }
}

onMounted(loadAuditLog)

function auditDetail(event: AuditEvent) {
  if (!event.detail_json) return ''
  try {
    return JSON.stringify(JSON.parse(event.detail_json))
  } catch {
    return event.detail_json
  }
}
</script>

<template>
  <section class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
      <div><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Durable work log</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Activity</h1><p class="text-muted-foreground mt-1.5">Snapshot, stream, poll, and repair outcomes from control-plane records.</p></div>
      <Select
        :model-value="activityDatabase || 'all'"
        @update:model-value="(value) => activityDatabase = value === 'all' ? '' : String(value)"
      >
        <SelectTrigger class="min-w-52"><ActivityIcon :size="15" /><SelectValue placeholder="All databases" /></SelectTrigger>
        <SelectContent>
          <SelectItem value="all">All databases</SelectItem>
          <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
        </SelectContent>
      </Select>
    </header>
    <Card class="overflow-hidden p-0">
      <div v-if="!filteredActivity.length" class="text-muted-foreground grid min-h-80 place-content-center justify-items-center gap-2 p-6 text-center"><ActivityIcon :size="28" /><h2 class="text-foreground font-semibold">No matching activity</h2><p class="max-w-md text-sm">Completed and failed replication work appears after the first snapshot.</p></div>
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
    <Card v-if="isAdmin" class="mt-4 overflow-hidden p-0">
      <div class="flex items-center justify-between gap-3 p-4 pb-0"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Every user, every workspace action</p><h2 class="text-base font-semibold">Audit trail</h2></div><Button variant="ghost" size="icon" :disabled="auditLoading" aria-label="Refresh audit trail" @click="loadAuditLog"><RefreshCw /></Button></div>
      <div v-if="!auditEvents.length" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 p-6 text-center"><ShieldCheck :size="26" /><strong class="text-foreground">No audit events yet</strong><span class="max-w-sm text-sm">Queries, configuration changes, and workspace management appear here.</span></div>
      <Table v-else>
        <TableHeader>
          <TableRow><TableHead>When</TableHead><TableHead>Actor</TableHead><TableHead>Action</TableHead><TableHead>Target</TableHead><TableHead>Detail</TableHead></TableRow>
        </TableHeader>
        <TableBody>
          <TableRow v-for="event in auditEvents" :key="event.id">
            <TableCell class="text-muted-foreground whitespace-nowrap">{{ formatDate(event.created_at) }}</TableCell>
            <TableCell><strong>{{ event.actor_label }}</strong><small class="text-muted-foreground block text-xs">{{ event.actor_type }}</small></TableCell>
            <TableCell><Badge variant="outline">{{ event.action }}</Badge></TableCell>
            <TableCell class="text-muted-foreground text-xs">{{ event.target_type ? `${event.target_type}:${event.target_id}` : '—' }}</TableCell>
            <TableCell class="text-muted-foreground max-w-96 truncate font-mono text-xs">{{ auditDetail(event) }}</TableCell>
          </TableRow>
        </TableBody>
      </Table>
    </Card>
  </section>
</template>
