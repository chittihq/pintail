<script setup lang="ts">
import { Database, Pause, Play, Plus, Trash2 } from '@lucide/vue'
import { dotToneClass, formatDate, messageOf, modeOf, stateTone } from '@/lib/format'
import type { DatabaseRecord } from '@/types/pintail'

const { databases, statuses, setMode, removeDatabase, error } = useControlPlane()

const deleteCandidate = ref<DatabaseRecord | null>(null)
const deleteText = ref('')

function closeDeleteDialog(open: boolean) {
  if (!open) {
    deleteCandidate.value = null
    deleteText.value = ''
  }
}

async function confirmRemove() {
  if (!deleteCandidate.value || deleteText.value !== deleteCandidate.value.name) return
  try {
    await removeDatabase(deleteCandidate.value.id)
    deleteCandidate.value = null
    deleteText.value = ''
  } catch (failure) {
    error.value = messageOf(failure)
  }
}
</script>

<template>
  <section class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7 flex flex-wrap items-start justify-between gap-4">
      <div><p class="text-muted-foreground mb-1.5 font-mono text-xs font-bold tracking-[0.12em] uppercase">Source registry</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Databases</h1><p class="text-muted-foreground mt-1.5">Every mirror has its own state, checkpoint, and failure boundary.</p></div>
      <Button as-child><NuxtLink to="/databases/new"><Plus /> Add database</NuxtLink></Button>
    </header>
    <Card class="overflow-hidden p-0">
      <div v-if="!databases.length" class="text-muted-foreground grid min-h-80 place-content-center justify-items-center gap-2 p-6 text-center">
        <Database :size="30" /><h2 class="text-foreground font-semibold">No databases yet</h2><p class="max-w-md text-sm">Connect a source, inspect its capabilities, and choose the tables to mirror.</p>
        <Button as-child><NuxtLink to="/databases/new">Start the connection wizard</NuxtLink></Button>
      </div>
      <Table v-else>
        <TableHeader>
          <TableRow>
            <TableHead>Name</TableHead>
            <TableHead>Mode</TableHead>
            <TableHead>State</TableHead>
            <TableHead>Rows</TableHead>
            <TableHead>Last event</TableHead>
            <TableHead><span class="sr-only">Actions</span></TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          <TableRow v-for="database in databases" :key="database.id">
            <TableCell>
              <NuxtLink :to="`/databases/${database.id}`" class="flex items-center gap-2.5">
                <span class="bg-accent text-accent-foreground grid size-8 place-items-center rounded-md border font-mono text-xs font-bold">{{ database.name.slice(0, 2).toUpperCase() }}</span><strong>{{ database.name }}</strong>
              </NuxtLink>
            </TableCell>
            <TableCell><Badge :class="`tone-${modeOf(database) === 'cdc' ? 'positive' : modeOf(database) === 'polling' ? 'warning' : 'neutral'}`">{{ modeOf(database) }}</Badge></TableCell>
            <TableCell><span class="flex items-center gap-2 capitalize"><span class="size-2 shrink-0 rounded-full" :class="dotToneClass(stateTone(database.state))" />{{ database.state }}</span></TableCell>
            <TableCell class="font-mono">{{ statuses[database.id]?.rows.toLocaleString() || 0 }}</TableCell>
            <TableCell class="text-muted-foreground">{{ formatDate(database.updated_at) }}</TableCell>
            <TableCell>
              <div class="flex items-center gap-1">
                <Button variant="ghost" size="icon-sm" :title="database.mode === 'paused' ? 'Resume' : 'Pause'" @click="setMode(database, database.mode === 'paused' ? 'auto' : 'paused')">
                  <Play v-if="database.mode === 'paused'" /><Pause v-else />
                </Button>
                <Button variant="ghost" size="icon-sm" title="Delete" @click="deleteCandidate = database; deleteText = ''"><Trash2 /></Button>
              </div>
            </TableCell>
          </TableRow>
        </TableBody>
      </Table>
    </Card>

    <Dialog :open="Boolean(deleteCandidate)" @update:open="closeDeleteDialog">
      <DialogContent>
        <DialogHeader>
          <div class="bg-red-soft text-red mb-2 flex size-11 items-center justify-center rounded-md"><Trash2 :size="20" /></div>
          <DialogTitle>Remove {{ deleteCandidate?.name }}?</DialogTitle>
          <DialogDescription>The source configuration is deleted. Mirrored storage is retained for manual recovery.</DialogDescription>
        </DialogHeader>
        <div class="grid gap-1.5">
          <Label for="delete-confirm">Type <strong>{{ deleteCandidate?.name }}</strong> to confirm</Label>
          <Input id="delete-confirm" v-model="deleteText" autocomplete="off" />
        </div>
        <DialogFooter>
          <Button variant="outline" @click="closeDeleteDialog(false)">Cancel</Button>
          <Button variant="destructive" :disabled="deleteText !== deleteCandidate?.name" @click="confirmRemove">Remove database</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  </section>
</template>
