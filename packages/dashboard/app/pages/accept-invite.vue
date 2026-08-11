<script setup lang="ts">
import { AlertTriangle, LoaderCircle, Mail } from '@lucide/vue'
import { messageOf } from '@/lib/format'
import type { InviteStatus } from '@/types/pintail'

const route = useRoute()
const { request } = usePintailApi()

const token = computed(() => (typeof route.query.token === 'string' ? route.query.token : ''))
const status = ref<InviteStatus | null>(null)
const loadError = ref('')
const loading = ref(true)

const REASON_MESSAGES: Record<string, string> = {
  not_found: 'This invite link is not valid.',
  revoked: 'This invite has been revoked.',
  accepted: 'This invite has already been accepted. Sign in normally instead.',
  expired: 'This invite has expired. Ask a workspace admin to send a new one.',
}

onMounted(async () => {
  if (!token.value) {
    loadError.value = 'Missing invite token.'
    loading.value = false
    return
  }
  try {
    status.value = await request<InviteStatus>(`/invites/status?token=${encodeURIComponent(token.value)}`)
  } catch (failure) {
    loadError.value = messageOf(failure)
  } finally {
    loading.value = false
  }
})

function signInWithGoogle() {
  // The token travels with the sign-in so the server redeems *this* invite.
  // Starting tokenless meant admission was resolved by searching every invite
  // for whatever address Google returned, which answers a different question
  // than "which invite did this person accept" - and answered it wrongly for
  // anyone who already had an account, or who had more than one invite open.
  window.location.href = `/api/auth/google/start?invite=${encodeURIComponent(token.value)}`
}
</script>

<template>
  <div class="w-full max-w-md">
    <Card class="p-6">
      <div v-if="loading" class="text-muted-foreground grid place-items-center gap-3 py-6"><LoaderCircle class="animate-spin" :size="22" /><span class="text-sm">Checking invite…</span></div>

      <div v-else-if="loadError || !status" class="grid place-items-center gap-3 py-4 text-center">
        <AlertTriangle class="text-destructive" :size="24" />
        <p class="text-sm">{{ loadError || 'Could not load this invite.' }}</p>
      </div>

      <div v-else-if="!status.valid" class="grid place-items-center gap-3 py-4 text-center">
        <AlertTriangle class="text-destructive" :size="24" />
        <p class="text-sm">{{ REASON_MESSAGES[status.reason || 'not_found'] }}</p>
      </div>

      <div v-else class="grid gap-5 text-center">
        <div class="grid place-items-center gap-2">
          <span class="bg-primary text-primary-foreground grid size-8 place-items-center font-mono text-xs font-extrabold">PT</span>
          <h1 class="text-xl font-bold">You're invited to {{ status.workspace_name }}</h1>
          <p class="text-muted-foreground text-balance text-sm">
            Sign in with the Google account for <strong class="text-foreground">{{ status.email }}</strong> to join as
            <Badge variant="outline">{{ status.role }}</Badge>.
          </p>
        </div>
        <Button class="w-full" @click="signInWithGoogle">
          <Mail /> Continue with Google
        </Button>
      </div>
    </Card>
  </div>
</template>
