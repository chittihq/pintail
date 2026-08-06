import type { DatabaseRecord, SnapshotStatus } from '@/types/pintail'

export function modeOf(database: DatabaseRecord) {
  return database.effective_mode || database.mode
}

export function stateTone(state: string) {
  if (['streaming', 'completed', 'ready'].includes(state)) return 'positive'
  if (['polling', 'snapshotting', 'running', 'probed'].includes(state)) return 'warning'
  if (['error', 'needs_resync'].includes(state)) return 'negative'
  return 'neutral'
}

export function dotToneClass(tone: string) {
  if (tone === 'positive') return 'bg-green'
  if (tone === 'warning') return 'bg-amber'
  if (tone === 'negative') return 'bg-destructive'
  return 'bg-muted-foreground'
}

export function formatNumber(value: number) {
  return new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 }).format(
    value,
  )
}

export function formatBytes(value: number) {
  if (value < 1_024) return `${value} B`
  if (value < 1_048_576) return `${(value / 1_024).toFixed(1)} KiB`
  return `${(value / 1_048_576).toFixed(1)} MiB`
}

export function formatDate(value: string | null) {
  return value ? new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(value)) : 'Never'
}

export function displayValue(value: unknown) {
  if (value === null) return 'NULL'
  if (typeof value === 'object') return JSON.stringify(value)
  return String(value)
}

export function messageOf(failure: unknown) {
  return failure instanceof Error ? failure.message : 'Unexpected control-plane error'
}

export function csvCell(value: unknown) {
  const text = displayValue(value)
  return `"${text.replaceAll('"', '""')}"`
}

export function snapshotPercent(table: SnapshotStatus['tables'][number]) {
  if (!table.total_chunks) return table.rows > 0 ? 100 : 0
  return Math.round((table.completed_chunks / table.total_chunks) * 100)
}

export function shellQuote(value: string) {
  return `'${value.replaceAll("'", "'\"'\"'")}'`
}

export function initials(subject: string) {
  return subject.split('@')[0]!.slice(0, 2).toUpperCase()
}
