<script setup lang="ts">
import { Activity, AlertTriangle, ChevronRight, Database, HardDrive, RefreshCw, Server, Radio } from '@lucide/vue'
import { dotToneClass, formatBytes, formatDate, formatNumber, modeOf, stateTone } from '@/lib/format'
import type { VitalsSample } from '@/composables/useVitals'

const { databases, statuses, activity, deadLetters, totalRows, activeMirrors, alertCount, loading, loadControlPlane, nodeStorage } = useControlPlane()

// One sample per second, for as long as this page is open. The stream is
// stopped on unmount so a backgrounded tab is not held open against the server.
const { samples, start, stop, WINDOW_SECONDS } = useVitals()
onMounted(start)
onBeforeUnmount(stop)

function gigabytes(bytes: number) {
  return bytes / 1024 ** 3
}

const memoryCaption = computed(() => {
  const limit = samples.value.at(-1)?.memory_limit_bytes
  return limit ? `of ${gigabytes(limit).toFixed(1)} GB limit` : 'no container limit'
})

// The card leads with the volume the data directory is on, always: that is
// what fills up and stops replication, and its used figure is the one that
// describes this node. The system volume follows on the second line when it
// is a different store. Leading with the system volume instead would have
// called a 91%-full macOS disk 3% used, because the sealed system volume
// counts only itself.
const storageVolume = computed(() => nodeStorage.value?.data ?? nodeStorage.value?.system ?? null)

/// Used share of the leading volume, as `df` computes its Capacity column:
/// against used + available rather than the raw total, so this figure and
/// the one an operator sees in a terminal agree. Null when unmeasurable -
/// which must not render as 0% free space.
const storageUsedPercent = computed(() => {
  const volume = storageVolume.value
  const occupied = (volume?.used_bytes ?? 0) + (volume?.available_bytes ?? 0)
  if (!volume || occupied === 0) return null
  return Math.round((volume.used_bytes / occupied) * 100)
})

const storageTone = computed(() => {
  const used = storageUsedPercent.value
  if (used === null) return 'tone-neutral'
  if (used >= 90) return 'tone-negative'
  if (used >= 75) return 'tone-warning'
  return 'tone-positive'
})

const storageCaption = computed(() => {
  const volume = storageVolume.value
  if (!volume) return 'Capacity unavailable'
  return `Free of ${formatBytes(volume.total_bytes)} on ${volume.mount}`
})

/// The second line carries whichever figure the first one is not: the
/// system's totals when the data directory is on its own volume, and
/// otherwise the path that volume is holding.
const storageDetail = computed(() => {
  const storage = nodeStorage.value
  if (!storage) return 'This node did not report its filesystems'
  if (storage.separate_mount && storage.system) {
    return `System volume: ${formatBytes(storage.system.available_bytes)} free of ${formatBytes(storage.system.total_bytes)}`
  }
  return `Data directory ${storage.data_dir}`
})
</script>

<template>
  <section class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
      <div>
        <p class="text-muted-foreground mb-1.5 font-mono text-xs font-bold tracking-[0.12em] uppercase">Live mirror fleet</p>
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
        <Button variant="outline" size="xs" class="shrink-0" as-child><NuxtLink to="/activity">Inspect</NuxtLink></Button>
      </AlertDescription>
    </Alert>

    <!-- Three live readings in one row: what this process is costing, and
         what it is doing with it. -->
    <div class="mb-4 grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
      <VitalsCard
        label="CPU"
        unit="%"
        :decimals="1"
        :max="100"
        color="var(--chart-1)"
        caption="of its core allowance"
        :window="WINDOW_SECONDS"
        :samples="samples"
        :value="(sample: VitalsSample) => sample.cpu_percent"
      />
      <VitalsCard
        label="Memory"
        unit="GB"
        :decimals="2"
        color="var(--chart-2)"
        :caption="memoryCaption"
        :window="WINDOW_SECONDS"
        :samples="samples"
        :value="(sample: VitalsSample) => gigabytes(sample.memory_bytes)"
      />
      <VitalsCard
        label="Queries"
        unit="/s"
        :decimals="2"
        color="var(--chart-3)"
        caption="read-only, this node"
        :window="WINDOW_SECONDS"
        :samples="samples"
        :value="(sample: VitalsSample) => sample.queries_per_second"
      />
    </div>

    <div class="@container/main">
      <div class="grid grid-cols-1 gap-4 *:data-[slot=card]:bg-gradient-to-t *:data-[slot=card]:from-primary/5 *:data-[slot=card]:to-card *:data-[slot=card]:shadow-xs @xl/main:grid-cols-2 @5xl/main:grid-cols-4 dark:*:data-[slot=card]:bg-card">
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
            <CardDescription>Storage</CardDescription>
            <CardTitle class="text-2xl font-semibold tabular-nums @[250px]/card:text-3xl">{{ storageVolume ? formatBytes(storageVolume.available_bytes) : '—' }}</CardTitle>
            <CardAction v-if="storageUsedPercent !== null">
              <Badge variant="outline" :class="storageTone">{{ storageUsedPercent }}% used</Badge>
            </CardAction>
          </CardHeader>
          <CardFooter class="flex-col items-start gap-1.5 text-sm">
            <div class="font-medium">{{ storageCaption }}</div>
            <div class="text-muted-foreground truncate" :title="storageDetail">{{ storageDetail }}</div>
          </CardFooter>
        </Card>
      </div>
    </div>

    <Card class="my-4 p-5">
      <div class="mb-5 flex flex-wrap items-center justify-between gap-3">
        <div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Signature path</p><h2 class="text-base font-semibold">Source → snapshot → stream</h2></div>
        <Badge variant="outline">Durable boundaries only</Badge>
      </div>
      <div class="grid grid-cols-[minmax(7rem,auto)_minmax(3rem,1fr)_minmax(7rem,auto)_minmax(3rem,1fr)_minmax(7rem,auto)] items-center gap-3 max-sm:grid-cols-1">
        <div class="text-muted-foreground grid justify-items-center gap-1 text-center"><Server :size="19" /><strong class="text-foreground text-sm">Source</strong><span class="font-mono text-xs">{{ databases.length }} configured</span></div>
        <div class="bg-border h-px overflow-hidden max-sm:mx-auto max-sm:h-8 max-sm:w-px"><span class="bg-foreground block h-full transition-[width]" :style="{ width: databases.length ? '100%' : '0%' }" /></div>
        <div class="text-muted-foreground grid justify-items-center gap-1 text-center"><HardDrive :size="19" /><strong class="text-foreground text-sm">Snapshot</strong><span class="font-mono text-xs">{{ databases.filter((item) => item.state === 'snapshotting').length }} running</span></div>
        <div class="bg-border h-px overflow-hidden max-sm:mx-auto max-sm:h-8 max-sm:w-px"><span class="bg-foreground block h-full transition-[width]" :style="{ width: activeMirrors ? '100%' : '0%' }" /></div>
        <div class="text-muted-foreground grid justify-items-center gap-1 text-center"><Radio :size="19" /><strong class="text-foreground text-sm">Stream</strong><span class="font-mono text-xs">{{ activeMirrors }} live</span></div>
      </div>
    </Card>

    <div class="grid gap-4 md:grid-cols-2">
      <Card class="p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><h2 class="text-base font-semibold">Database lag posture</h2><Button variant="link" size="sm" as-child><NuxtLink to="/databases">View all</NuxtLink></Button></div>
        <div v-if="!databases.length && !loading" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 text-center">
          <Database :size="24" /><strong class="text-foreground">No source connected</strong><span class="max-w-sm text-sm">Add MySQL to begin the first mirror.</span>
          <Button as-child><NuxtLink to="/databases/new">Add database</NuxtLink></Button>
        </div>
        <div v-else class="divide-y">
          <NuxtLink v-for="database in databases" :key="database.id" :to="`/databases/${database.id}`" class="hover:bg-accent flex w-full items-center gap-3 py-2.5 text-left">
            <span class="bg-accent text-accent-foreground grid size-8 shrink-0 place-items-center rounded-md border font-mono text-xs font-bold">{{ database.name.slice(0, 2).toUpperCase() }}</span>
            <span class="grid min-w-0 flex-1"><strong class="truncate">{{ database.name }}</strong><small class="text-muted-foreground text-xs">{{ statuses[database.id]?.rows.toLocaleString() || 0 }} rows</small></span>
            <Badge :class="`tone-${stateTone(database.state)}`">{{ modeOf(database) }}</Badge>
            <ChevronRight :size="15" class="text-muted-foreground shrink-0" />
          </NuxtLink>
        </div>
      </Card>
      <Card class="p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><h2 class="text-base font-semibold">Latest activity</h2><Button variant="link" size="sm" as-child><NuxtLink to="/activity">Open log</NuxtLink></Button></div>
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
</template>
