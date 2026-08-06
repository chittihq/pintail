<script setup lang="ts">
import { ArrowRight, LoaderCircle, Radio } from '@lucide/vue'
import { useIntervalFn } from '@vueuse/core'
import { toast } from 'vue-sonner'
import 'vue-sonner/style.css'
import { messageOf } from '@/lib/format'
import type { Session } from '@/types/pintail'

const { token, restoreToken, setToken, request } = usePintailApi()
const {
  session,
  loading,
  dark,
  loadNodeStatus,
  loadControlPlane,
  startEventStream,
  refreshLiveData,
  applyTheme,
} = useControlPlane()

const authMode = ref<'setup' | 'login'>('login')
const authenticating = ref(false)
const booting = ref(true)
const error = ref('')
const authForm = reactive({ email: '', password: '' })

useHead({
  bodyAttrs: { class: 'min-h-screen' },
})

onMounted(async () => {
  restoreToken()
  dark.value = window.localStorage.getItem('pintail.theme') === 'dark'
  applyTheme()
  await loadNodeStatus()
  try {
    const setup = await request<{ required: boolean }>('/auth/setup/status')
    authMode.value = setup.required ? 'setup' : 'login'
    if (token.value) {
      session.value = await request<Session>('/session')
      await loadControlPlane()
      startEventStream()
    }
  } catch {
    setToken(null)
  } finally {
    booting.value = false
  }
})

useIntervalFn(() => {
  if (session.value && !loading.value) void refreshLiveData()
}, 8_000)

async function submitAuth() {
  authenticating.value = true
  error.value = ''
  try {
    const response = await request<{
      token: string
      user: { id: string; email: string; role: string }
    }>(`/auth/${authMode.value}`, {
      method: 'POST',
      body: JSON.stringify(authForm),
    })
    setToken(response.token)
    session.value = await request<Session>('/session')
    await loadControlPlane()
    startEventStream()
    toast(authMode.value === 'setup' ? 'Operator initialized' : 'Signed in')
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    authenticating.value = false
  }
}
</script>

<template>
  <Toaster position="top-right" />

  <div v-if="booting" class="text-muted-foreground flex min-h-svh items-center justify-center gap-3 font-mono text-xs tracking-wide uppercase" aria-live="polite">
    <div class="bg-primary text-primary-foreground grid size-8 place-items-center font-mono text-[0.65rem] font-extrabold">PT</div>
    <LoaderCircle class="animate-spin" :size="20" />
    <span>Opening control plane</span>
  </div>

  <main v-else-if="!session" class="bg-muted flex min-h-svh items-center justify-center p-6 md:p-10">
    <div class="w-full max-w-4xl">
      <Card class="overflow-hidden p-0">
        <CardContent class="grid p-0 md:grid-cols-2">
          <form class="p-6 md:p-8" @submit.prevent="submitAuth">
            <div class="flex flex-col gap-6">
              <div class="flex flex-col items-center gap-2 text-center">
                <span class="bg-primary text-primary-foreground grid size-8 place-items-center font-mono text-[0.65rem] font-extrabold">PT</span>
                <h1 class="text-xl font-bold">{{ authMode === 'setup' ? 'Create the operator' : 'Welcome back' }}</h1>
                <p class="text-muted-foreground text-balance">
                  {{
                    authMode === 'setup'
                      ? 'This one-time account owns source configuration, replication, and access keys.'
                      : 'Authenticate to inspect and operate your live MySQL mirrors.'
                  }}
                </p>
              </div>
              <div class="grid gap-1.5">
                <Label for="auth-email">Email</Label>
                <Input id="auth-email" v-model="authForm.email" type="email" autocomplete="email" required placeholder="operator@example.com" />
              </div>
              <div class="grid gap-1.5">
                <Label for="auth-password">Password</Label>
                <Input
                  id="auth-password"
                  v-model="authForm.password"
                  type="password"
                  :autocomplete="authMode === 'setup' ? 'new-password' : 'current-password'"
                  minlength="12"
                  required
                  placeholder="At least 12 characters"
                />
              </div>
              <p v-if="error" class="text-destructive text-sm">{{ error }}</p>
              <Button type="submit" class="w-full" :disabled="authenticating">
                <LoaderCircle v-if="authenticating" class="animate-spin" />
                {{ authMode === 'setup' ? 'Initialize Pintail' : 'Sign in' }}
                <ArrowRight v-if="!authenticating" />
              </Button>
              <p class="text-muted-foreground text-center text-xs">Credentials stay on this Pintail node · Argon2id protected</p>
            </div>
          </form>
          <aside class="relative hidden min-h-[22rem] place-items-center overflow-hidden bg-neutral-950 text-neutral-300 md:grid" aria-hidden="true">
            <div class="grid w-[min(70%,44rem)] grid-cols-4 gap-2 p-8 [transform:perspective(60rem)_rotateX(54deg)_rotateZ(-28deg)]">
              <span
                v-for="index in 28"
                :key="index"
                class="min-h-20 border border-neutral-700 bg-neutral-800 shadow-[0_1.2rem_2rem_rgba(0,0,0,0.17)]"
                :class="{ 'border-neutral-100 bg-neutral-100': [7, 14, 21, 22].includes(index) }"
              />
            </div>
            <div class="absolute bottom-8 left-8 flex items-center gap-3 font-mono text-xs tracking-wide">
              <Radio :size="18" />
              <span>Source events become durable analytical blocks.</span>
            </div>
          </aside>
        </CardContent>
      </Card>
    </div>
  </main>

  <NuxtLayout v-else>
    <NuxtPage />
  </NuxtLayout>
</template>
