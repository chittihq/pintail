<script setup lang="ts">
import { Copy, Mail, Trash2, UserPlus, Users, X } from '@lucide/vue'
import { toast } from 'vue-sonner'
import { formatDate, messageOf } from '@/lib/format'
import type { CreatedInvite, Invite, WorkspaceMember } from '@/types/pintail'

const { request } = usePintailApi()
const { session, error } = useControlPlane()

const members = ref<WorkspaceMember[]>([])
const invites = ref<Invite[]>([])
const inviteForm = reactive({ email: '', role: 'operator' })
const creatingInvite = ref(false)
const revealedInvite = ref<CreatedInvite | null>(null)

async function loadTeam() {
  try {
    const [memberRows, inviteRows] = await Promise.all([
      request<WorkspaceMember[]>('/workspaces/members'),
      request<Invite[]>('/workspaces/invites'),
    ])
    members.value = memberRows
    invites.value = inviteRows
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

onMounted(loadTeam)

function inviteStatus(invite: Invite) {
  if (invite.revoked_at) return { label: 'revoked', tone: 'neutral' }
  if (invite.accepted_at) return { label: 'accepted', tone: 'positive' }
  if (new Date(invite.expires_at) <= new Date()) return { label: 'expired', tone: 'negative' }
  return { label: 'pending', tone: 'warning' }
}

function inviteLink(token: string) {
  return `${window.location.origin}/accept-invite?token=${token}`
}

async function createInvite() {
  if (!inviteForm.email.trim()) return
  creatingInvite.value = true
  try {
    const invite = await request<CreatedInvite>('/workspaces/invites', {
      method: 'POST',
      body: JSON.stringify({ email: inviteForm.email.trim(), role: inviteForm.role }),
    })
    revealedInvite.value = invite
    inviteForm.email = ''
    await loadTeam()
  } catch (failure) {
    error.value = messageOf(failure)
  } finally {
    creatingInvite.value = false
  }
}

async function revokeInvite(invite: Invite) {
  try {
    await request(`/workspaces/invites/${invite.id}`, { method: 'DELETE' })
    toast('Invite revoked')
    await loadTeam()
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

async function removeMember(member: WorkspaceMember) {
  try {
    await request(`/workspaces/members/${member.user_id}`, { method: 'DELETE' })
    toast(`${member.email} removed from workspace`)
    await loadTeam()
  } catch (failure) {
    error.value = messageOf(failure)
  }
}

async function copy(value: string) {
  await navigator.clipboard.writeText(value)
  toast('Invite link copied')
}
</script>

<template>
  <section class="mx-auto w-full max-w-[88rem] px-4 py-10 sm:px-6">
    <header class="mb-7">
      <p class="text-muted-foreground mb-1.5 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Workspace access</p>
      <h1 class="text-2xl font-bold tracking-tight sm:text-3xl">Team</h1>
      <p class="text-muted-foreground mt-1.5">Invite teammates by email; they sign in with Google once the invite is accepted.</p>
    </header>

    <Card class="mb-4 grid items-end gap-4 p-4 sm:grid-cols-[1.2fr_1fr_auto]">
      <div class="grid content-start gap-1.5">
        <Label for="invite-email">Email</Label>
        <Input id="invite-email" v-model="inviteForm.email" type="email" placeholder="teammate@company.com" />
      </div>
      <div class="grid content-start gap-1.5">
        <Label>Role</Label>
        <Select v-model="inviteForm.role">
          <SelectTrigger class="w-full"><SelectValue /></SelectTrigger>
          <SelectContent>
            <SelectItem value="admin">Admin</SelectItem>
            <SelectItem value="operator">Operator</SelectItem>
            <SelectItem value="viewer">Viewer</SelectItem>
          </SelectContent>
        </Select>
      </div>
      <Button :disabled="creatingInvite || !inviteForm.email.trim()" @click="createInvite"><UserPlus /> Invite</Button>
    </Card>

    <Alert v-if="revealedInvite" class="mb-4">
      <Mail />
      <AlertDescription class="flex w-full items-center gap-3">
        <div class="flex-1">
          <strong class="text-foreground block">Copy this link and send it to {{ revealedInvite.email }}. It cannot be recovered.</strong>
          <code class="mt-1 block break-all">{{ inviteLink(revealedInvite.token) }}</code>
        </div>
        <Button variant="ghost" size="icon-sm" class="shrink-0" @click="copy(inviteLink(revealedInvite.token))"><Copy /></Button>
        <Button variant="ghost" size="icon-sm" class="shrink-0" @click="revealedInvite = null"><X /></Button>
      </AlertDescription>
    </Alert>

    <div class="grid gap-4 md:grid-cols-2">
      <Card class="overflow-hidden p-0">
        <div class="p-4 pb-0"><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Active</p><h2 class="text-base font-semibold">Members</h2></div>
        <div v-if="!members.length" class="text-muted-foreground grid min-h-40 place-content-center justify-items-center gap-2 p-6 text-center"><Users :size="24" /><span class="text-sm">No members yet</span></div>
        <Table v-else>
          <TableHeader><TableRow><TableHead>Email</TableHead><TableHead>Role</TableHead><TableHead><span class="sr-only">Actions</span></TableHead></TableRow></TableHeader>
          <TableBody>
            <TableRow v-for="member in members" :key="member.user_id">
              <TableCell><strong>{{ member.email }}</strong></TableCell>
              <TableCell><Badge variant="outline">{{ member.role }}</Badge></TableCell>
              <TableCell>
                <Button
                  v-if="member.user_id !== session?.subject"
                  variant="ghost"
                  size="icon-sm"
                  aria-label="Remove member"
                  @click="removeMember(member)"
                ><Trash2 /></Button>
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>
      </Card>

      <Card class="overflow-hidden p-0">
        <div class="p-4 pb-0"><p class="text-muted-foreground mb-1 font-mono text-[0.63rem] font-bold tracking-[0.12em] uppercase">Pending and past</p><h2 class="text-base font-semibold">Invites</h2></div>
        <div v-if="!invites.length" class="text-muted-foreground grid min-h-40 place-content-center justify-items-center gap-2 p-6 text-center"><Mail :size="24" /><span class="text-sm">No invites sent</span></div>
        <Table v-else>
          <TableHeader><TableRow><TableHead>Email</TableHead><TableHead>Role</TableHead><TableHead>Status</TableHead><TableHead>Expires</TableHead><TableHead><span class="sr-only">Actions</span></TableHead></TableRow></TableHeader>
          <TableBody>
            <TableRow v-for="invite in invites" :key="invite.id">
              <TableCell><strong>{{ invite.email }}</strong></TableCell>
              <TableCell><Badge variant="outline">{{ invite.role }}</Badge></TableCell>
              <TableCell><Badge :class="`tone-${inviteStatus(invite).tone}`">{{ inviteStatus(invite).label }}</Badge></TableCell>
              <TableCell class="text-muted-foreground">{{ formatDate(invite.expires_at) }}</TableCell>
              <TableCell>
                <Button
                  v-if="!invite.accepted_at && !invite.revoked_at"
                  variant="ghost"
                  size="icon-sm"
                  aria-label="Revoke invite"
                  @click="revokeInvite(invite)"
                ><Trash2 /></Button>
              </TableCell>
            </TableRow>
          </TableBody>
        </Table>
      </Card>
    </div>
  </section>
</template>
