<script setup lang="ts">
import { AlertTriangle, Copy, Database, KeyRound, LoaderCircle, Plus, Trash2, X } from '@lucide/vue'
import { toast } from 'vue-sonner'
import { copyText, formatDate, messageOf } from '@/lib/format'
import type { ApiKeyRecord } from '@/types/pintail'

const route = useRoute()
const router = useRouter()
const { request } = usePintailApi()
const { databases, error } = useControlPlane()

const keyDatabaseId = computed({
  get: () => (typeof route.query.db === 'string' ? route.query.db : databases.value[0]?.id || ''),
  set: (value) => router.replace({ query: { ...route.query, db: value } }),
})
const keys = ref<ApiKeyRecord[]>([])
const keysLoading = ref(false)
const keyForm = reactive({ name: '', scopes: ['read', 'query'] })
const revealedSecret = ref('')

async function loadKeys() {
  if (!keyDatabaseId.value) {
    keys.value = []
    return
  }
  keysLoading.value = true
  try {
    keys.value = await request<ApiKeyRecord[]>(`/databases/${keyDatabaseId.value}/api-keys`)
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    keysLoading.value = false
  }
}

watch(keyDatabaseId, loadKeys, { immediate: true })

async function createKey() {
  if (!keyDatabaseId.value || !keyForm.name.trim()) return
  try {
    const key = await request<ApiKeyRecord>(`/databases/${keyDatabaseId.value}/api-keys`, {
      method: 'POST',
      body: JSON.stringify({ name: keyForm.name, scopes: keyForm.scopes }),
    })
    revealedSecret.value = key.secret || ''
    keyForm.name = ''
    await loadKeys()
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

// Both of these reload from the server rather than mutating the local row,
// so a failed request leaves the table showing what the server still holds.
// Without the catch the rejection was unhandled and the row simply stayed as
// it was, which reads as "the click did nothing" - the same silent failure
// the create path already avoids.
async function toggleKey(key: ApiKeyRecord) {
  try {
    await request(`/databases/${key.database_id}/api-keys/${key.id}`, {
      method: 'PATCH',
      body: JSON.stringify({ enabled: !key.enabled }),
    })
    await loadKeys()
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

const deleteKeyCandidate = ref<ApiKeyRecord | null>(null)
const deletingKey = ref(false)

async function deleteKey() {
  const key = deleteKeyCandidate.value
  if (!key || deletingKey.value) return
  deletingKey.value = true
  try {
    await request(`/databases/${key.database_id}/api-keys/${key.id}`, { method: 'DELETE' })
    toast(`${key.name} deleted; clients holding it lose access now`)
    deleteKeyCandidate.value = null
    await loadKeys()
  } catch (failure) {
    error.value = messageOf(failure)
    toast(`Deleting ${key.name} failed: ${messageOf(failure)}`)
  } finally {
    deletingKey.value = false
  }
}

function toggleScope(scope: string, on: boolean) {
  keyForm.scopes = on
    ? [...new Set([...keyForm.scopes, scope])]
    : keyForm.scopes.filter((existing) => existing !== scope)
}

</script>

<template>
  <section class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
      <div><p class="text-muted-foreground mb-1.5 font-mono text-xs font-bold tracking-[0.12em] uppercase">Database-scoped access</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">API Keys</h1><p class="text-muted-foreground mt-1.5">Secrets are SHA-256 hash-only and shown once.</p></div>
      <Select v-model="keyDatabaseId">
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
      <Button :disabled="!keyDatabaseId || !keyForm.name.trim() || !keyForm.scopes.length" @click="createKey"><Plus /> Create</Button>
    </Card>
    <Alert v-if="revealedSecret" class="mb-4">
      <AlertTriangle />
      <AlertDescription class="flex w-full items-center gap-3">
        <div class="flex-1"><strong class="text-foreground block">Copy this secret now. It cannot be recovered.</strong><code data-testid="revealed-secret" class="mt-1 block break-all">{{ revealedSecret }}</code></div>
        <Button variant="ghost" size="icon-sm" class="shrink-0" aria-label="Copy secret" @click="copyText(revealedSecret)"><Copy /></Button>
        <Button variant="ghost" size="icon-sm" class="shrink-0" aria-label="Dismiss secret" @click="revealedSecret = ''"><X /></Button>
      </AlertDescription>
    </Alert>
    <Card class="overflow-hidden p-0">
      <div v-if="!keys.length && keysLoading" class="text-muted-foreground grid min-h-80 place-content-center justify-items-center p-6"><LoaderCircle class="animate-spin" :size="24" /></div>
      <div v-else-if="!keys.length" class="text-muted-foreground grid min-h-80 place-content-center justify-items-center gap-2 p-6 text-center"><KeyRound :size="28" /><h2 class="text-foreground font-semibold">No keys for this database</h2><p class="max-w-md text-sm">Create one for the HTTP API or MySQL wire clients.</p></div>
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
                <Button variant="ghost" size="icon-sm" :aria-label="`Delete ${key.name}`" @click="deleteKeyCandidate = key"><Trash2 /></Button>
              </div>
            </TableCell>
          </TableRow>
        </TableBody>
      </Table>
    </Card>

    <ConfirmActionDialog
      :open="Boolean(deleteKeyCandidate)"
      :title="`Delete ${deleteKeyCandidate?.name}?`"
      description="The secret is hash-only and cannot be re-issued. Every client using this key loses access immediately. To stop a key temporarily, use Disable instead."
      confirm-label="Delete key"
      :working="deletingKey"
      @confirm="deleteKey"
      @update:open="(open) => { if (!open) deleteKeyCandidate = null }"
    />
  </section>
</template>
