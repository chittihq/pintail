<script setup lang="ts">
import { Archive, Database, HardDrive, LoaderCircle, Play, RefreshCw } from '@lucide/vue'
import { useIntervalFn } from '@vueuse/core'
import { toast } from 'vue-sonner'
import { formatBytes, formatDate, messageOf, stateTone } from '@/lib/format'
import type { BackupConfig, BackupRecord } from '@/types/pintail'

const route = useRoute()
const router = useRouter()
const { request } = usePintailApi()
const { databases, error } = useControlPlane()

const backupDatabaseId = computed({
  get: () => (typeof route.query.db === 'string' ? route.query.db : databases.value[0]?.id || ''),
  set: (value) => router.replace({ query: { ...route.query, db: value } }),
})
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

async function loadBackups(showLoading = true) {
  if (!backupDatabaseId.value) {
    backups.value = []
    backupConfigLoaded.value = false
    return
  }
  if (showLoading) backupLoading.value = true
  try {
    backups.value = await request<BackupRecord[]>(`/databases/${backupDatabaseId.value}/backups`)
    const completed = backups.value.find((backup) => backup.status === 'completed')
    if (!restoreBackupId.value || !backups.value.some((backup) => backup.id === restoreBackupId.value)) {
      restoreBackupId.value = completed?.id || ''
    }
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    if (showLoading) backupLoading.value = false
  }
}

/// Separate from the artifact refresh on purpose: refreshing history after
/// "Backup now" used to overwrite the WHOLE form from the server, silently
/// discarding whatever the user had typed - including an unsaved secret key.
/// The form now reloads only when the database changes or a save completes.
async function loadBackupConfig() {
  if (!backupDatabaseId.value) return
  try {
    const config = await request<BackupConfig>(`/databases/${backupDatabaseId.value}/backup-config`)
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
  }
}

watch(backupDatabaseId, () => Promise.all([loadBackups(), loadBackupConfig()]), { immediate: true })

// A backup left "running" used to stay that way on screen until the user
// found the refresh icon; success and failure both arrived silently. While
// any run is live the history refreshes itself and announces the outcome.
const watchedRuns = ref(new Set<string>())
useIntervalFn(async () => {
  const running = backups.value.filter((backup) => backup.status === 'running')
  for (const run of running) watchedRuns.value.add(run.id)
  if (!running.length) return
  await loadBackups(false)
  for (const backup of backups.value) {
    if (!watchedRuns.value.has(backup.id) || backup.status === 'running') continue
    watchedRuns.value.delete(backup.id)
    if (backup.status === 'completed') toast(`Backup completed: ${formatBytes(backup.bytes)} uploaded`)
    else toast(`Backup ${backup.status}: ${backup.error || 'see the history table'}`)
  }
}, 5_000)

async function saveBackupConfig() {
  if (!backupDatabaseId.value || !backupForm.bucket.trim() || !backupForm.prefix.trim()) return
  backupLoading.value = true
  try {
    await request<BackupConfig>(`/databases/${backupDatabaseId.value}/backup-config`, {
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
    })
    toast('Backup destination saved')
    await loadBackupConfig()
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
  const { loadControlPlane } = useControlPlane()
  if (!backupDatabaseId.value || !restoreBackupId.value || !restoreName.value.trim()) return
  backupLoading.value = true
  try {
    await request(`/databases/${backupDatabaseId.value}/backups/restore`, {
      method: 'POST',
      body: JSON.stringify({ backup_id: restoreBackupId.value, name: restoreName.value.trim() }),
      // A large restore legitimately outlives the 30s control-plane
      // deadline; aborting client-side while the server continues invites a
      // retry that restores a SECOND database.
      timeoutMs: 600_000,
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
</script>

<template>
  <section class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
      <div><p class="text-muted-foreground mb-1.5 font-mono text-xs font-bold tracking-[0.12em] uppercase">Recovery plane</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Backups</h1><p class="text-muted-foreground mt-1.5">Checksum-verified manifests, immutable segments, and control-plane state restore side-by-side.</p></div>
      <Select v-model="backupDatabaseId">
        <SelectTrigger class="min-w-52"><Database :size="15" /><SelectValue placeholder="Choose database" /></SelectTrigger>
        <SelectContent>
          <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
        </SelectContent>
      </Select>
    </header>
    <div class="grid gap-4 md:grid-cols-2">
      <Card class="grid gap-4 p-4">
        <div class="flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">S3-compatible destination</p><h2 class="text-base font-semibold">Backup configuration</h2></div><Badge :class="backupConfigLoaded ? 'tone-positive' : 'tone-neutral'">{{ backupConfigLoaded ? 'Configured' : 'Not configured' }}</Badge></div>
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
        <div class="flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Manual recovery point</p><h2 class="text-base font-semibold">Backup now</h2></div><Archive :size="19" class="text-muted-foreground" /></div>
        <p class="text-muted-foreground text-sm">The first run is full. Later runs reuse unchanged immutable segment objects unless you force a new full chain.</p>
        <div class="flex items-center gap-2">
          <Button :disabled="backupLoading || !backupConfigLoaded" @click="runBackup(false)"><Play /> Backup now</Button>
          <Button variant="outline" :disabled="backupLoading || !backupConfigLoaded" @click="runBackup(true)"><RefreshCw /> Force full</Button>
        </div>
        <Separator />
        <div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Side-by-side restore</p><h2 class="text-base font-semibold">Restore as new database</h2></div>
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
      <div class="flex items-center justify-between gap-3 p-4 pb-0"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Durable audit</p><h2 class="text-base font-semibold">Backup history</h2></div><Button variant="ghost" size="icon" :disabled="backupLoading" aria-label="Refresh backup history" @click="loadBackups()"><RefreshCw /></Button></div>
      <div v-if="!backups.length && backupLoading" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center p-6"><LoaderCircle class="animate-spin" :size="22" /></div>
      <div v-else-if="!backups.length" class="text-muted-foreground grid min-h-48 place-content-center justify-items-center gap-2 p-6 text-center"><Archive :size="26" /><strong class="text-foreground">No backup artifacts</strong><span class="max-w-sm text-sm">Save a destination, then create the first full recovery point.</span></div>
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
</template>
