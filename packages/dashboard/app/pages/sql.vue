<script setup lang="ts">
import { Database, LoaderCircle, Play, Search, SquareTerminal } from '@lucide/vue'
import { displayValue, messageOf } from '@/lib/format'
import type { QueryResponse } from '@/types/pintail'

const route = useRoute()
const router = useRouter()
const { request } = usePintailApi()
const { databases } = useControlPlane()

const sqlDatabaseId = computed({
  get: () => (typeof route.query.db === 'string' ? route.query.db : databases.value[0]?.id || ''),
  set: (value) => router.replace({ query: { ...route.query, db: value } }),
})
const sqlText = ref('SELECT *\nFROM events\nLIMIT 100')
const sqlResult = ref<QueryResponse | null>(null)
const sqlRunning = ref(false)
const sqlError = ref('')

if (typeof route.query.describe === 'string') {
  sqlText.value = `DESCRIBE \`${route.query.describe.replaceAll('`', '``')}\``
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

</script>

<template>
  <section class="mx-auto flex w-full max-w-[88rem] flex-col px-4 py-10 sm:px-6">
    <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
      <div><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Native query engine</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">SQL Console</h1><p class="text-muted-foreground mt-1.5">MySQL dialect over reader-pinned columnar snapshots.</p></div>
      <Select v-model="sqlDatabaseId">
        <SelectTrigger class="min-w-52"><Database :size="15" /><SelectValue placeholder="Choose database" /></SelectTrigger>
        <SelectContent>
          <SelectItem v-for="database in databases" :key="database.id" :value="database.id">{{ database.name }}</SelectItem>
        </SelectContent>
      </Select>
    </header>
    <Card v-if="!databases.length" class="text-muted-foreground grid min-h-80 place-content-center justify-items-center gap-2 p-6 text-center"><SquareTerminal :size="30" /><h2 class="text-foreground font-semibold">No queryable mirror</h2><p class="max-w-md text-sm">Add and snapshot a database before opening the console.</p><Button as-child><NuxtLink to="/databases/new">Add database</NuxtLink></Button></Card>
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
        <div class="border-b p-4">
          <h2 class="text-base font-semibold">Results</h2>
          <p v-if="sqlResult" class="text-muted-foreground mt-1 font-mono text-xs">{{ sqlResult.stats.rows }} rows · {{ sqlResult.stats.duration_ms }} ms · {{ sqlResult.stats.blocks_read }} blocks read / {{ sqlResult.stats.blocks_pruned }} pruned</p>
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
</template>
