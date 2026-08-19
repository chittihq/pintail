import { toast } from 'vue-sonner'
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
  if (value < 1_073_741_824) return `${(value / 1_048_576).toFixed(1)} MiB`
  return `${(value / 1_073_741_824).toFixed(1)} GiB`
}

export function formatDate(value: string | null) {
  if (!value) return 'Never'
  const parsed = new Date(value)
  // Intl throws RangeError on an invalid Date, and this renders inside table
  // cells - one bad timestamp from the server must not blank a whole page.
  if (Number.isNaN(parsed.getTime())) return value
  return new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(parsed)
}

export function displayValue(value: unknown) {
  if (value === null) return 'NULL'
  if (typeof value === 'object') return JSON.stringify(value)
  return String(value)
}

/// The clipboard API rejects on plain-HTTP deploys, denied permissions, and
/// unfocused documents. The callers hand it show-once secrets - an API key,
/// an invite link - so a silent failure here means the user dismisses a
/// secret believing it is on the clipboard, and it is gone.
export async function copyText(value: string, label = 'Copied to clipboard') {
  try {
    await navigator.clipboard.writeText(value)
    toast(label)
  } catch {
    toast('Copy failed - select the text and copy it by hand')
  }
}

export function messageOf(failure: unknown) {
  return failure instanceof Error ? failure.message : 'Unexpected control-plane error'
}

export function csvCell(value: unknown) {
  let text = displayValue(value)
  // A cell starting with = + - @ or a tab executes as a formula when the CSV
  // is opened in a spreadsheet; a leading apostrophe forces it to read as
  // text (the standard neutralization, and what spreadsheets themselves do).
  if (/^[=+\-@\t]/.test(text)) text = `'${text}`
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
