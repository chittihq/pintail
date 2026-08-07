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
  loadWorkspaces,
  loadControlPlane,
  startEventStream,
  refreshLiveData,
  applyTheme,
} = useControlPlane()

const route = useRoute()
const router = useRouter()

const authMode = ref<'setup' | 'login'>('login')
const authenticating = ref(false)
const booting = ref(true)
const error = ref('')
const authForm = reactive({ email: '', password: '' })
const googleEnabled = ref(false)

const AUTH_ERROR_MESSAGES: Record<string, string> = {
  not_invited: 'That Google account has not been invited to a workspace.',
  invalid_request: 'The sign-in attempt was invalid or expired. Try again.',
  account_disabled: 'This account is disabled.',
  link_required: 'Sign in with your existing method, then link Google from Settings.',
  sign_in_failed: 'Google sign-in failed. Try again.',
}

// `/accept-invite` must render before anyone has an account — it is how a
// brand-new invitee gets one. Every other page requires a session.
const isPublicRoute = computed(() => route.path === '/accept-invite')

useHead({
  bodyAttrs: { class: 'min-h-screen' },
})

async function loadGoogleStatus() {
  try {
    googleEnabled.value = (await request<{ enabled: boolean }>('/auth/google/status')).enabled
  } catch {
    googleEnabled.value = false
  }
}

function signInWithGoogle() {
  window.location.href = '/api/auth/google/start'
}

onMounted(async () => {
  restoreToken()
  const authCode = typeof route.query.auth_code === 'string' ? route.query.auth_code : null
  const authError = typeof route.query.auth_error === 'string' ? route.query.auth_error : null
  if (authCode || authError) {
    const { auth_code: _authCode, auth_error: _authError, ...rest } = route.query
    await router.replace({ path: route.path, query: rest })
  }
  let googleSignedIn = false
  let googleLinked = false
  if (authCode) {
    setToken(null)
    try {
      const response = await request<{ token: string, outcome: 'signed_in' | 'linked' }>('/auth/google/exchange', {
        method: 'POST',
        body: JSON.stringify({ code: authCode }),
      })
      setToken(response.token)
      googleLinked = response.outcome === 'linked'
      googleSignedIn = !googleLinked
    } catch (failure) {
      error.value = messageOf(failure)
    }
  }
  if (authError) error.value = AUTH_ERROR_MESSAGES[authError] || 'Google sign-in failed. Try again.'

  dark.value = window.localStorage.getItem('pintail.theme') === 'dark'
  applyTheme()
  await loadNodeStatus()
  void loadGoogleStatus()

  try {
    const setup = await request<{ required: boolean }>('/auth/setup/status')
    authMode.value = setup.required ? 'setup' : 'login'
    if (token.value) {
      session.value = await request<Session>('/session')
      await loadWorkspaces()
      await loadControlPlane()
      startEventStream()
      if (googleLinked) toast('Google account linked')
      else if (googleSignedIn) toast('Signed in with Google')
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
    await loadWorkspaces()
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

  <main v-else-if="isPublicRoute" class="bg-muted flex min-h-svh items-center justify-center p-6 md:p-10">
    <NuxtPage />
  </main>

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
              <template v-if="googleEnabled && authMode === 'login'">
                <div class="relative text-center text-xs after:absolute after:inset-0 after:top-1/2 after:z-0 after:flex after:items-center after:border-t"><span class="bg-card text-muted-foreground relative z-10 px-2">or</span></div>
                <Button type="button" variant="outline" class="w-full" @click="signInWithGoogle">
                  <svg viewBox="0 0 24 24" class="size-4"><path fill="#4285F4" d="M23.5 12.27c0-.82-.07-1.6-.2-2.36H12v4.47h6.47c-.28 1.5-1.13 2.77-2.4 3.62v3h3.87c2.27-2.09 3.56-5.17 3.56-8.73Z"/><path fill="#34A853" d="M12 24c3.24 0 5.96-1.07 7.94-2.9l-3.87-3c-1.08.72-2.45 1.15-4.07 1.15-3.13 0-5.78-2.11-6.73-4.96H1.27v3.1C3.24 21.3 7.3 24 12 24Z"/><path fill="#FBBC05" d="M5.27 14.29a7.2 7.2 0 0 1 0-4.58v-3.1H1.27a12 12 0 0 0 0 10.78l4-3.1Z"/><path fill="#EA4335" d="M12 4.75c1.76 0 3.34.6 4.58 1.79l3.44-3.44C17.95 1.19 15.24 0 12 0 7.3 0 3.24 2.7 1.27 6.61l4 3.1C6.22 6.86 8.87 4.75 12 4.75Z"/></svg>
                  Continue with Google
                </Button>
              </template>
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
