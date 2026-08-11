<script setup lang="ts">
import {
  Activity,
  AlertTriangle,
  Archive,
  Cable,
  Check,
  ChevronRight,
  ChevronsUpDown,
  Database,
  KeyRound,
  LayoutDashboard,
  LoaderCircle,
  LogOut,
  Moon,
  Plus,
  Settings as SettingsIcon,
  SquareTerminal,
  Sun,
  Users,
  X,
} from '@lucide/vue'
import { toast } from 'vue-sonner'
import { initials, messageOf } from '@/lib/format'

type NavItem = { to: string; label: string; icon: unknown; activeOn: (path: string) => boolean, adminOnly?: boolean }

const nav: NavItem[] = [
  { to: '/', label: 'Overview', icon: LayoutDashboard, activeOn: (path) => path === '/' },
  { to: '/databases', label: 'Databases', icon: Database, activeOn: (path) => path.startsWith('/databases') },
  { to: '/sql', label: 'SQL Console', icon: SquareTerminal, activeOn: (path) => path === '/sql' },
  { to: '/activity', label: 'Activity', icon: Activity, activeOn: (path) => path === '/activity' },
  { to: '/keys', label: 'API Keys', icon: KeyRound, activeOn: (path) => path === '/keys' },
  { to: '/backups', label: 'Backups', icon: Archive, activeOn: (path) => path === '/backups' },
  { to: '/team', label: 'Team', icon: Users, activeOn: (path) => path === '/team', adminOnly: true },
  { to: '/settings', label: 'Settings', icon: SettingsIcon, activeOn: (path) => path === '/settings' },
  { to: '/connect', label: 'Connect', icon: Cable, activeOn: (path) => path === '/connect' },
]

const route = useRoute()
const {
  session,
  workspaces,
  error,
  dark,
  alertCount,
  databases,
  logout,
  toggleTheme,
  switchWorkspace,
  createWorkspace,
} = useControlPlane()

const currentWorkspace = computed(
  () => workspaces.value.find((item) => item.id === session.value?.workspace_id) ?? null,
)
const isAdmin = computed(() => session.value?.role === 'admin')
const visibleNav = computed(() => nav.filter((item) => !item.adminOnly || isAdmin.value))

const currentLabel = computed(() => {
  if (route.path.startsWith('/databases/') && route.params.id) {
    const database = databases.value.find((item) => item.id === route.params.id)
    if (database) return database.name
  }
  return nav.find((item) => item.activeOn(route.path))?.label || 'Pintail'
})

const createWorkspaceOpen = ref(false)
const newWorkspaceName = ref('')
const creatingWorkspace = ref(false)

async function onSwitchWorkspace(workspaceId: string) {
  if (workspaceId === session.value?.workspace_id) return
  await switchWorkspace(workspaceId)
  await navigateTo('/')
}

async function submitCreateWorkspace() {
  if (!newWorkspaceName.value.trim()) return
  creatingWorkspace.value = true
  try {
    await createWorkspace(newWorkspaceName.value.trim())
    createWorkspaceOpen.value = false
    newWorkspaceName.value = ''
    await navigateTo('/')
  } catch (failure) {
    toast(messageOf(failure))
  } finally {
    creatingWorkspace.value = false
  }
}

function signOut() {
  logout()
  navigateTo('/')
}
</script>

<template>
  <SidebarProvider :style="{ '--sidebar-width': 'calc(var(--spacing) * 64)', '--header-height': 'calc(var(--spacing) * 14)' }">
    <Sidebar variant="inset" collapsible="icon">
      <SidebarHeader>
        <SidebarMenu>
          <SidebarMenuItem>
            <DropdownMenu>
              <DropdownMenuTrigger as-child>
                <SidebarMenuButton size="lg" class="data-[slot=sidebar-menu-button]:!p-1.5 data-[state=open]:bg-sidebar-accent">
                  <span class="bg-primary text-primary-foreground grid size-7 shrink-0 place-items-center font-mono text-xs font-extrabold">PT</span>
                  <div class="grid flex-1 text-left text-sm leading-tight">
                    <span class="truncate text-base font-extrabold tracking-tight">Pintail</span>
                    <span class="text-sidebar-foreground/60 truncate text-sm">{{ currentWorkspace?.name || 'Choose a workspace' }}</span>
                  </div>
                  <ChevronsUpDown class="ml-auto size-4" />
                </SidebarMenuButton>
              </DropdownMenuTrigger>
              <DropdownMenuContent class="w-(--reka-dropdown-menu-trigger-width) min-w-56 rounded-lg" side="bottom" align="start" :side-offset="4">
                <DropdownMenuLabel class="text-muted-foreground font-mono text-xs tracking-[0.1em] uppercase">Workspaces</DropdownMenuLabel>
                <DropdownMenuItem
                  v-for="workspace in workspaces"
                  :key="workspace.id"
                  @click="onSwitchWorkspace(workspace.id)"
                >
                  <span class="bg-accent text-accent-foreground grid size-6 shrink-0 place-items-center rounded font-mono text-xs font-bold uppercase">{{ workspace.name.slice(0, 2) }}</span>
                  <span class="flex-1 truncate">{{ workspace.name }}</span>
                  <Check v-if="workspace.id === session?.workspace_id" class="text-foreground" />
                </DropdownMenuItem>
                <DropdownMenuSeparator />
                <DropdownMenuItem @click="createWorkspaceOpen = true">
                  <Plus /> Create workspace
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarHeader>
      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              <SidebarMenuItem v-for="item in visibleNav" :key="item.to">
                <SidebarMenuButton as-child :is-active="item.activeOn(route.path)" :tooltip="item.label">
                  <NuxtLink :to="item.to">
                    <component :is="item.icon" />
                    <span>{{ item.label }}</span>
                  </NuxtLink>
                </SidebarMenuButton>
                <SidebarMenuBadge
                  v-if="item.to === '/activity' && alertCount"
                  class="bg-red rounded-full font-mono text-xs text-white"
                >
                  {{ alertCount }}
                </SidebarMenuBadge>
              </SidebarMenuItem>
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
      <SidebarFooter v-if="session">
        <SidebarMenu>
          <SidebarMenuItem>
            <DropdownMenu>
              <DropdownMenuTrigger as-child>
                <SidebarMenuButton size="lg" class="data-[state=open]:bg-sidebar-accent data-[state=open]:text-sidebar-accent-foreground">
                  <Avatar class="size-8 rounded-md">
                    <AvatarFallback class="rounded-md">{{ initials(session.subject) }}</AvatarFallback>
                  </Avatar>
                  <div class="grid flex-1 text-left text-sm leading-tight">
                    <span class="flex items-center gap-1.5 truncate font-medium">
                      <span class="size-2 shrink-0 rounded-full" :class="error ? 'bg-destructive' : 'bg-green'" />
                      {{ error ? 'Attention' : 'Node healthy' }}
                    </span>
                    <span class="text-sidebar-foreground/60 truncate text-sm">{{ session.subject }}</span>
                  </div>
                  <ChevronsUpDown class="ml-auto size-4" />
                </SidebarMenuButton>
              </DropdownMenuTrigger>
              <DropdownMenuContent class="w-(--reka-dropdown-menu-trigger-width) min-w-56 rounded-lg" side="top" align="start" :side-offset="4">
                <DropdownMenuLabel class="p-0 font-normal">
                  <div class="flex items-center gap-2 px-1 py-1.5 text-left text-sm">
                    <Avatar class="size-8 rounded-md">
                      <AvatarFallback class="rounded-md">{{ initials(session.subject) }}</AvatarFallback>
                    </Avatar>
                    <div class="grid flex-1 text-left text-sm leading-tight">
                      <span class="truncate font-medium">{{ session.subject }}</span>
                      <span class="text-muted-foreground truncate text-xs">{{ session.role }} · v0.1.0</span>
                    </div>
                  </div>
                </DropdownMenuLabel>
                <DropdownMenuSeparator />
                <DropdownMenuItem as-child>
                  <NuxtLink to="/settings"><SettingsIcon /> Settings</NuxtLink>
                </DropdownMenuItem>
                <DropdownMenuSeparator />
                <DropdownMenuItem @click="signOut">
                  <LogOut /> Sign out
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarFooter>
      <SidebarRail />
    </Sidebar>

    <SidebarInset class="min-w-0">
      <header class="flex h-(--header-height) shrink-0 items-center gap-2 border-b transition-[width,height] ease-linear">
        <div class="flex w-full items-center gap-1 px-4 lg:gap-2 lg:px-6">
          <SidebarTrigger class="-ml-1" />
          <Separator orientation="vertical" class="mx-2 data-[orientation=vertical]:h-4" />
          <div class="text-muted-foreground flex items-center gap-1.5 text-xs">
            <span>Control plane</span>
            <ChevronRight :size="14" />
            <strong class="text-foreground">{{ currentLabel }}</strong>
          </div>
          <div class="ml-auto flex items-center gap-2">
            <Button variant="ghost" size="icon" :title="dark ? 'Use light theme' : 'Use dark theme'" @click="toggleTheme">
              <Sun v-if="dark" />
              <Moon v-else />
            </Button>
            <Button as-child><NuxtLink to="/databases/new"><Plus /> <span class="hidden sm:inline">Add database</span></NuxtLink></Button>
          </div>
        </div>
      </header>

      <Alert v-if="error" variant="destructive" class="rounded-none border-x-0">
        <AlertTriangle />
        <AlertDescription class="flex w-full items-center justify-between gap-3">
          <span>{{ error }}</span>
          <Button variant="ghost" size="icon-xs" class="shrink-0" @click="error = ''"><X /></Button>
        </AlertDescription>
      </Alert>

      <slot />
    </SidebarInset>

    <Dialog v-model:open="createWorkspaceOpen">
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Create a workspace</DialogTitle>
          <DialogDescription>You become its admin. Databases, members, and activity are scoped to this workspace alone.</DialogDescription>
        </DialogHeader>
        <form class="grid gap-1.5" @submit.prevent="submitCreateWorkspace">
          <Label for="workspace-name">Name</Label>
          <Input id="workspace-name" v-model="newWorkspaceName" required placeholder="Acme production" autofocus />
        </form>
        <DialogFooter>
          <Button variant="outline" @click="createWorkspaceOpen = false">Cancel</Button>
          <Button :disabled="creatingWorkspace || !newWorkspaceName.trim()" @click="submitCreateWorkspace">
            <LoaderCircle v-if="creatingWorkspace" class="animate-spin" /> Create workspace
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  </SidebarProvider>
</template>
