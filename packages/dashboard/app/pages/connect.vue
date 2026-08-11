<script setup lang="ts">
import { ArrowRight, CircleHelp, Copy, Download, Radio, ShieldCheck } from '@lucide/vue'
import { toast } from 'vue-sonner'
import { messageOf, shellQuote } from '@/lib/format'

const route = useRoute()
const router = useRouter()
const { databases, nodeStatus } = useControlPlane()
const { token } = usePintailApi()

const keyDatabaseId = computed({
  get: () => (typeof route.query.db === 'string' ? route.query.db : databases.value[0]?.id || ''),
  set: (value) => router.replace({ query: { ...route.query, db: value } }),
})
const selectedConnectDatabase = computed(
  () => databases.value.find((database) => database.id === keyDatabaseId.value) ?? databases.value[0] ?? null,
)
const connectKey = ref('pk_your_key')
const connectHost = ref(import.meta.client ? window.location.hostname || '127.0.0.1' : '127.0.0.1')
const connectPort = ref(nodeStatus.value?.wire.port ? String(nodeStatus.value.wire.port) : '3306')

async function copy(value: string) {
  await navigator.clipboard.writeText(value)
  toast('Copied to clipboard')
}

function connectSnippet(kind: 'mysql' | 'node' | 'python') {
  const database = selectedConnectDatabase.value?.name || 'analytics'
  const host = connectHost.value || '127.0.0.1'
  const port = Math.min(65_535, Math.max(1, Number.parseInt(connectPort.value, 10) || 3306))
  if (kind === 'node') {
    return `// bun add mysql2
import { readFileSync } from 'node:fs'
import mysql from 'mysql2/promise'

const db = await mysql.createConnection({
  host: ${JSON.stringify(host)},
  port: ${port},
  user: ${JSON.stringify(database)},
  password: ${JSON.stringify(connectKey.value)},
  database: ${JSON.stringify(database)},
  // mysql2 verifies by default and will reject the node's self-signed
  // certificate without this. Download it from the card above.
  ssl: { ca: readFileSync('pintail-ca.pem') },
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
    ssl={"ca": "pintail-ca.pem"},
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
  --database=${shellQuote(database)} \\
  --ssl-mode=VERIFY_CA --ssl-ca=pintail-ca.pem`
}
async function downloadCertificate() {
  try {
    const response = await fetch('/api/wire/certificate', {
      headers: { Authorization: `Bearer ${token.value}` },
    })
    if (!response.ok) throw new Error(await response.text())
    const url = URL.createObjectURL(await response.blob())
    const anchor = document.createElement('a')
    anchor.href = url
    anchor.download = 'pintail-ca.pem'
    anchor.click()
    URL.revokeObjectURL(url)
  } catch (failure) {
    toast(messageOf(failure))
  }
}
</script>

<template>
  <section class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7"><p class="text-muted-foreground mb-1.5 font-mono text-xs font-bold tracking-[0.12em] uppercase">Client handoff</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Connect to Pintail</h1><p class="text-muted-foreground mt-1.5">The database name is the username; its scoped API key is the password.</p></header>
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
      <Button variant="link" size="sm" class="max-sm:col-span-2 max-sm:justify-self-start" as-child><NuxtLink to="/keys">Create or rotate key <ArrowRight /></NuxtLink></Button>
    </Card>
    <!-- The certificate is the public half; the private key never leaves the
         node. Downloading it is what turns an encrypted connection into a
         verified one. -->
    <Card class="mb-4 grid grid-cols-[auto_1fr_auto] items-center gap-3 p-4 max-sm:grid-cols-[auto_1fr]">
      <ShieldCheck :size="17" class="text-muted-foreground" />
      <div class="grid gap-0.5">
        <strong class="text-sm">TLS certificate</strong>
        <span class="text-muted-foreground text-xs">This node issues its own. Connections are encrypted without it; download it to also verify you are talking to this node and not something in between.</span>
      </div>
      <Button variant="outline" size="sm" class="max-sm:col-span-2 max-sm:justify-self-start" @click="downloadCertificate">
        <Download /> Download certificate
      </Button>
    </Card>

    <div class="grid gap-4 sm:grid-cols-2">
      <Card v-for="kind in (['mysql', 'node', 'python'] as const)" :key="kind" class="overflow-hidden p-0">
        <div class="flex items-center justify-between gap-3 p-4 pb-3"><h2 class="text-base font-semibold">{{ kind === 'mysql' ? 'MySQL CLI' : kind === 'node' ? 'Node.js' : 'Python' }}</h2><Button variant="ghost" size="icon" @click="copy(connectSnippet(kind))"><Copy /></Button></div>
        <pre class="bg-muted overflow-auto p-3.5 text-xs leading-relaxed break-all whitespace-pre-wrap">{{ connectSnippet(kind) }}</pre>
      </Card>
      <Card class="p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><h2 class="text-base font-semibold">DBeaver / Metabase</h2><CircleHelp :size="17" class="text-muted-foreground" /></div>
        <dl class="mb-4 grid grid-cols-2 gap-x-4">
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Driver</dt><dd class="mt-1 text-sm">MySQL 8</dd></div>
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Host / port</dt><dd class="mt-1 text-sm">{{ connectHost }}:{{ connectPort }}</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Database / user</dt><dd class="mt-1 text-sm">{{ selectedConnectDatabase?.name || 'analytics' }}</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Password</dt><dd class="mt-1 text-sm">Query-scoped API key</dd></div>
        </dl>
        <p class="text-muted-foreground text-xs leading-relaxed">Keep SSL disabled for a loopback endpoint. Terminate TLS at your private ingress when clients connect across a network.</p>
      </Card>
    </div>
  </section>
</template>
