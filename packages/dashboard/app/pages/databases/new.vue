<script setup lang="ts">
import { AlertTriangle, ArrowRight, Check, LoaderCircle, Radio, X } from '@lucide/vue'
import { messageOf } from '@/lib/format'
import type { DatabaseRecord, ProbeReport } from '@/types/pintail'

const { request } = usePintailApi()
const { loadControlPlane } = useControlPlane()

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
    await navigateTo(`/databases/${wizard.databaseId}?tab=snapshot`)
  } catch (failure) {
    wizard.error = messageOf(failure)
  } finally {
    wizard.working = false
  }
}

function toggleInclude(name: string, on: boolean) {
  wizard.includes = on
    ? [...new Set([...wizard.includes, name])]
    : wizard.includes.filter((existing) => existing !== name)
}
</script>

<template>
  <section class="mx-auto w-full max-w-4xl px-4 py-10 sm:px-6">
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
          <Button type="button" variant="outline" as-child><NuxtLink to="/databases">Cancel</NuxtLink></Button>
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
</template>
