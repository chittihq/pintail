<script setup lang="ts">
import { Moon, Server, Sun } from '@lucide/vue'

const { session, dark, nodeStatus, toggleTheme } = useControlPlane()
</script>

<template>
  <section v-if="session" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7"><p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Node policy</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Settings</h1><p class="text-muted-foreground mt-1.5">Operator identity, network surfaces, and local presentation.</p></header>
    <div class="grid items-start gap-4 sm:grid-cols-2">
      <Card class="p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Operator</p><h2 class="text-base font-semibold">Current session</h2></div><Server :size="19" class="text-muted-foreground" /></div>
        <dl class="grid grid-cols-2 gap-x-4">
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Subject</dt><dd class="mt-1 font-mono text-sm">{{ session.subject }}</dd></div>
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Role</dt><dd class="mt-1 text-sm">{{ session.role }}</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Scopes</dt><dd class="mt-1 text-sm">{{ session.scopes.join(', ') }}</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Session</dt><dd class="mt-1 text-sm">12-hour signed JWT</dd></div>
        </dl>
      </Card>
      <Card class="p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Appearance</p><h2 class="text-base font-semibold">Interface</h2></div><Button variant="ghost" size="icon" @click="toggleTheme"><Sun v-if="dark" /><Moon v-else /></Button></div>
        <div class="flex w-full items-center justify-between py-1">
          <span><strong class="block text-sm">Dark instrument panel</strong><small class="text-muted-foreground text-xs">Stored only in this browser.</small></span>
          <Switch :model-value="dark" @update:model-value="() => toggleTheme()" />
        </div>
      </Card>
      <Card class="overflow-hidden p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">MySQL wire</p><h2 class="text-base font-semibold">Client endpoint</h2></div><Badge :class="nodeStatus?.wire.enabled ? 'tone-positive' : 'tone-negative'">{{ nodeStatus?.wire.enabled ? 'Live' : 'Unavailable' }}</Badge></div>
        <div class="bg-muted mb-3 flex items-center gap-2.5 rounded-md border p-3">
          <span class="size-2 shrink-0 rounded-full" :class="nodeStatus?.wire.enabled ? 'bg-green' : 'bg-destructive'" />
          <code class="truncate text-sm">{{ nodeStatus?.wire.bind || 'Endpoint unavailable' }}</code>
        </div>
        <dl class="grid grid-cols-2 gap-x-4">
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Mode</dt><dd class="mt-1 text-sm">Read-only</dd></div>
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Authentication</dt><dd class="mt-1 text-sm">Database API key</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Username</dt><dd class="mt-1 text-sm">Database name</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Protocol</dt><dd class="mt-1 text-sm">MySQL native</dd></div>
        </dl>
      </Card>
      <Card class="p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Telemetry</p><h2 class="text-base font-semibold">Operations</h2></div><Badge class="tone-positive">Live</Badge></div>
        <dl class="grid grid-cols-2 gap-x-4">
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Metrics</dt><dd class="mt-1 text-sm"><a href="/metrics" target="_blank" class="underline underline-offset-2">/metrics</a></dd></div>
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Format</dt><dd class="mt-1 text-sm">Prometheus text</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Supervisor</dt><dd class="mt-1 text-sm">Isolated per database</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-[0.57rem] uppercase">Recovery</dt><dd class="mt-1 text-sm">Scheduled + manual</dd></div>
        </dl>
      </Card>
    </div>
  </section>
</template>
