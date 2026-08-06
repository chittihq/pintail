<script setup lang="ts">
import { AlertTriangle, Copy, Database, KeyRound, Plus, Trash2, X } from '@lucide/vue'
import { toast } from 'vue-sonner'
import { formatDate, messageOf } from '@/lib/format'
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
const keyForm = reactive({ name: '', scopes: ['read', 'query'] })
const revealedSecret = ref('')

async function loadKeys() {
  if (!keyDatabaseId.value) {
    keys.value = []
    return
  }
  try {
    keys.value = await request<ApiKeyRecord[]>(`/databases/${keyDatabaseId.value}/api-keys`)
  } catch (failure) {
    error.value = messageOf(failure)
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

async function toggleKey(key: ApiKeyRecord) {
  await request(`/databases/${key.database_id}/api-keys/${key.id}`, {
    method: 'PATCH',
    body: JSON.stringify({ enabled: !key.enabled }),
  })
  await loadKeys()
}

async function deleteKey(key: ApiKeyRecord) {
  await request(`/databases/${key.database_id}/api-keys/${key.id}`, { method: 'DELETE' })
  await loadKeys()
}

function toggleScope(scope: string, on: boolean) {
  keyForm.scopes = on
    ? [...new Set([...keyForm.scopes, scope])]
    : keyForm.scopes.filter((existing) => existing !== scope)
}

async function copy(value: string) {
  await navigator.clipboard.writeText(value)
  toast('Copied to clipboard')
}
</script>

<template>
  <section class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
      <div><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Database-scoped access</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">API Keys</h1><p class="text-muted-foreground mt-1.5">Secrets are SHA-256 hash-only and shown once.</p></div>
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
</template>
