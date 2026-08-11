<script setup lang="ts">
import { KeyRound, Link2, LoaderCircle, Moon, Server, ShieldCheck, Sun } from '@lucide/vue'
import { toast } from 'vue-sonner'
import { messageOf } from '@/lib/format'
import type { GoogleOAuthSettings } from '@/types/pintail'

const { session, dark, nodeStatus, toggleTheme, error } = useControlPlane()
const { request } = usePintailApi()

const isAdmin = computed(() => session.value?.role === 'admin')
const googleLoaded = ref(false)
const googleSaving = ref(false)
const googleLinking = ref(false)
const googleEnabled = ref(false)
const googleForm = reactive({ enabled: false, clientId: '', clientSecret: '', publicUrl: '' })
const googleConfigured = ref(false)

async function loadGoogleSettings() {
  if (!isAdmin.value) return
  try {
    const settings = await request<GoogleOAuthSettings>('/settings/oauth/google')
    googleForm.enabled = settings.enabled
    googleForm.clientId = settings.client_id
    googleForm.publicUrl = settings.public_url
    googleForm.clientSecret = ''
    googleConfigured.value = settings.configured
    googleLoaded.value = true
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

async function loadGoogleStatus() {
  try {
    googleEnabled.value = (await request<{ enabled: boolean }>('/auth/google/status')).enabled
  } catch {
    googleEnabled.value = false
  }
}

onMounted(async () => {
  await Promise.all([loadGoogleSettings(), loadGoogleStatus()])
})

async function linkGoogle() {
  googleLinking.value = true
  try {
    const response = await request<{ authorization_url: string }>('/settings/oauth/google/link', {
      method: 'POST',
    })
    window.location.assign(response.authorization_url)
  } catch (failure) {
    error.value = messageOf(failure)
    googleLinking.value = false
  }
}

/// Mirrors normalize_public_origin on the server. Duplicated deliberately:
/// the server stays authoritative and still rejects a bad value, but a whole
/// save failing on a 400 leaves the toggle looking like it silently turned
/// itself off and the credentials unsaved, with the reason buried in a
/// generic error line. Naming the problem on the field is the difference
/// between a five second fix and a puzzle.
function validateDomainUrl(value: string, required: boolean): string {
  const trimmed = value.trim()
  if (!trimmed) return required ? 'A public URL is required to enable Google sign-in.' : ''
  let url: URL
  try {
    url = new URL(trimmed)
  } catch {
    return 'Must be an absolute URL including the scheme, for example https://pintail.example.com'
  }
  const loopback = ['localhost', '127.0.0.1', '[::1]'].includes(url.hostname)
  if (url.protocol !== 'https:' && !(url.protocol === 'http:' && loopback)) {
    return 'Must use HTTPS. Only localhost may use http.'
  }
  if (url.username || url.password || (url.pathname !== '' && url.pathname !== '/') || url.search || url.hash) {
    return 'Only scheme, host and optional port - no path, query, credentials or fragment.'
  }
  return ''
}

const domainUrlError = computed(() => validateDomainUrl(googleForm.publicUrl, googleForm.enabled))

async function saveGoogleSettings() {
  googleSaving.value = true
  try {
    const settings = await request<GoogleOAuthSettings>('/settings/oauth/google', {
      method: 'PUT',
      body: JSON.stringify({
        enabled: googleForm.enabled,
        client_id: googleForm.clientId.trim(),
        client_secret: googleForm.clientSecret || undefined,
        public_url: googleForm.publicUrl.trim(),
      }),
    })
    googleForm.clientSecret = ''
    googleConfigured.value = settings.configured
    toast('Google sign-in settings saved')
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    googleSaving.value = false
  }
}
interface WireTlsSettings {
  hostnames: string
  active_names: string[]
  restart_required: boolean
}

const wireTls = ref<WireTlsSettings | null>(null)
const wireTlsForm = reactive({ hostnames: '' })
const wireTlsSaving = ref(false)

async function loadWireTls() {
  if (!isAdmin.value) return
  try {
    wireTls.value = await request<WireTlsSettings>('/settings/wire-tls')
    wireTlsForm.hostnames = wireTls.value.hostnames
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

async function saveWireTls() {
  wireTlsSaving.value = true
  try {
    wireTls.value = await request<WireTlsSettings>('/settings/wire-tls', {
      method: 'PUT',
      body: JSON.stringify({ hostnames: wireTlsForm.hostnames }),
    })
    wireTlsForm.hostnames = wireTls.value.hostnames
    toast(wireTls.value.restart_required
      ? 'Saved. The certificate is reissued on the next restart.'
      : 'Certificate hostnames saved')
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    wireTlsSaving.value = false
  }
}

onMounted(loadWireTls)
</script>

<template>
  <section v-if="session" class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7"><p class="text-muted-foreground mb-1.5 font-mono text-xs font-bold tracking-[0.12em] uppercase">Node policy</p><h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Settings</h1><p class="text-muted-foreground mt-1.5">Operator identity, network surfaces, and local presentation.</p></header>
    <div class="grid items-start gap-4 sm:grid-cols-2">
      <Card class="p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Operator</p><h2 class="text-base font-semibold">Current session</h2></div><Server :size="19" class="text-muted-foreground" /></div>
        <dl class="grid grid-cols-2 gap-x-4">
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Subject</dt><dd class="mt-1 font-mono text-sm">{{ session.subject }}</dd></div>
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Role</dt><dd class="mt-1 text-sm">{{ session.role }}</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Scopes</dt><dd class="mt-1 text-sm">{{ session.scopes.join(', ') }}</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Session</dt><dd class="mt-1 text-sm">12-hour signed JWT</dd></div>
        </dl>
        <Button v-if="googleEnabled" class="mt-4" variant="outline" :disabled="googleLinking" @click="linkGoogle"><LoaderCircle v-if="googleLinking" class="animate-spin" /><Link2 v-else /> Link Google account</Button>
      </Card>
      <Card class="p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Appearance</p><h2 class="text-base font-semibold">Interface</h2></div><Button variant="ghost" size="icon" @click="toggleTheme"><Sun v-if="dark" /><Moon v-else /></Button></div>
        <div class="flex w-full items-center justify-between py-1">
          <span><strong class="block text-sm">Dark instrument panel</strong><small class="text-muted-foreground text-xs">Stored only in this browser.</small></span>
          <Switch :model-value="dark" @update:model-value="() => toggleTheme()" />
        </div>
      </Card>
      <Card class="overflow-hidden p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">MySQL wire</p><h2 class="text-base font-semibold">Client endpoint</h2></div><Badge :class="nodeStatus?.wire.enabled ? 'tone-positive' : 'tone-negative'">{{ nodeStatus?.wire.enabled ? 'Live' : 'Unavailable' }}</Badge></div>
        <div class="bg-muted mb-3 flex items-center gap-2.5 rounded-md border p-3">
          <span class="size-2 shrink-0 rounded-full" :class="nodeStatus?.wire.enabled ? 'bg-green' : 'bg-destructive'" />
          <code class="truncate text-sm">{{ nodeStatus?.wire.bind || 'Endpoint unavailable' }}</code>
        </div>
        <!-- The certificate is read once at boot, so this deliberately does
             not claim to take effect on save. -->
        <div v-if="isAdmin" class="mb-3 grid gap-2 rounded-md border p-3">
          <div class="grid gap-1.5">
            <Label for="wire-hostnames">Certificate hostnames</Label>
            <Input id="wire-hostnames" v-model="wireTlsForm.hostnames" placeholder="pintail.example.com" />
            <small class="text-muted-foreground text-xs">
              Comma-separated. Clients verifying with <code>VERIFY_IDENTITY</code> must dial one of these names.
              <code>localhost</code>, <code>127.0.0.1</code> and <code>::1</code> are always included.
            </small>
          </div>
          <div v-if="wireTls?.active_names?.length" class="text-muted-foreground text-xs">
            The live certificate covers: <code>{{ wireTls.active_names.join(', ') }}</code>
          </div>
          <p v-if="wireTls?.restart_required" data-testid="wire-tls-restart" class="text-destructive text-xs">
            Saved, but the running certificate does not cover these names yet. It is reissued on the next restart, which
            invalidates the certificate anyone has already downloaded.
          </p>
          <Button size="sm" class="justify-self-start" :disabled="wireTlsSaving" @click="saveWireTls">
            <LoaderCircle v-if="wireTlsSaving" class="animate-spin" /><ShieldCheck v-else /> Save hostnames
          </Button>
        </div>

        <dl class="grid grid-cols-2 gap-x-4">
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Mode</dt><dd class="mt-1 text-sm">Read-only</dd></div>
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Authentication</dt><dd class="mt-1 text-sm">Database API key</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Username</dt><dd class="mt-1 text-sm">Database name</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Protocol</dt><dd class="mt-1 text-sm">MySQL native</dd></div>
        </dl>
      </Card>
      <Card v-if="isAdmin" class="grid gap-4 p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Node-wide, one OAuth client covers every workspace</p><h2 class="text-base font-semibold">Google sign-in</h2></div><Badge :class="googleConfigured ? 'tone-positive' : 'tone-neutral'">{{ googleConfigured ? 'Configured' : 'Not configured' }}</Badge></div>
        <div class="grid gap-3">
          <div class="grid content-start gap-1.5"><Label for="google-public-url">Public URL</Label><Input id="google-public-url" v-model="googleForm.publicUrl" inputmode="url" autocomplete="url" placeholder="https://pintail.example.com" :aria-invalid="Boolean(domainUrlError)" /><small v-if="domainUrlError" data-testid="domain-url-error" class="text-destructive text-xs">{{ domainUrlError }}</small><small v-else class="text-muted-foreground text-xs">The fixed origin registered for the Google callback; forwarded host headers are ignored.</small></div>
          <div class="grid content-start gap-1.5"><Label for="google-client-id">Client ID</Label><Input id="google-client-id" v-model="googleForm.clientId" autocomplete="off" placeholder="123456789-abc.apps.googleusercontent.com" /></div>
          <div class="grid content-start gap-1.5"><Label for="google-client-secret">Client secret</Label><Input id="google-client-secret" v-model="googleForm.clientSecret" type="password" autocomplete="new-password" placeholder="Leave blank to preserve" /></div>
        </div>
        <div class="flex w-full items-center justify-between py-1">
          <span><strong class="block text-sm">Allow sign-in with Google</strong><small class="text-muted-foreground text-xs">Invited identities sign in directly; existing accounts link explicitly above.</small></span>
          <Switch :model-value="googleForm.enabled" @update:model-value="(value) => googleForm.enabled = value === true" />
        </div>
        <Button :disabled="googleSaving || !googleLoaded || Boolean(domainUrlError)" @click="saveGoogleSettings"><LoaderCircle v-if="googleSaving" class="animate-spin" /><KeyRound v-else /> Save Google settings</Button>
      </Card>
      <Card class="p-4">
        <div class="mb-4 flex items-center justify-between gap-3"><div><p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">Telemetry</p><h2 class="text-base font-semibold">Operations</h2></div><Badge class="tone-positive">Live</Badge></div>
        <dl class="grid grid-cols-2 gap-x-4">
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Metrics</dt><dd class="mt-1 text-sm"><a href="/metrics" target="_blank" class="underline underline-offset-2">/metrics</a></dd></div>
          <div class="border-b py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Format</dt><dd class="mt-1 text-sm">Prometheus text</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Supervisor</dt><dd class="mt-1 text-sm">Isolated per database</dd></div>
          <div class="py-3"><dt class="text-muted-foreground font-mono text-xs uppercase">Recovery</dt><dd class="mt-1 text-sm">Scheduled + manual</dd></div>
        </dl>
      </Card>
    </div>
  </section>
</template>
