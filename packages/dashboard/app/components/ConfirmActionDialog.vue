<script setup lang="ts">
import { LoaderCircle, Trash2 } from '@lucide/vue'

/// One confirmation gate for the destructive one-click actions: deleting an
/// API key, removing a member, discarding a dead letter. Each of these sits
/// one mis-click from an irreversible loss (the key page even places the
/// trash icon beside the everyday Disable link), and none of them had any
/// guard - the click WAS the deletion.
defineProps<{
  open: boolean
  title: string
  description: string
  confirmLabel: string
  working?: boolean
}>()

const emit = defineEmits<{ 'confirm': []; 'update:open': [value: boolean] }>()
</script>

<template>
  <Dialog :open="open" @update:open="(value) => emit('update:open', value)">
    <DialogContent>
      <DialogHeader>
        <div class="bg-red-soft text-red mb-2 flex size-11 items-center justify-center rounded-md"><Trash2 :size="20" /></div>
        <DialogTitle>{{ title }}</DialogTitle>
        <DialogDescription>{{ description }}</DialogDescription>
      </DialogHeader>
      <DialogFooter>
        <Button variant="outline" @click="emit('update:open', false)">Cancel</Button>
        <Button variant="destructive" :disabled="working" @click="emit('confirm')"><LoaderCircle v-if="working" class="animate-spin" /> {{ confirmLabel }}</Button>
      </DialogFooter>
    </DialogContent>
  </Dialog>
</template>
