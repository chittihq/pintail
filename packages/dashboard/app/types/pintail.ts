export interface Session {
  subject: string
  role: string
  database_id: string | null
  scopes: string[]
}

export interface DatabaseRecord {
  id: string
  name: string
  mode: 'auto' | 'cdc' | 'polling' | 'paused'
  effective_mode: 'cdc' | 'polling' | 'paused' | null
  state: string
  include_tables: string[]
  exclude_tables: string[]
  poll_interval_seconds: number
  reconcile_interval_seconds: number
  created_at: string
  updated_at: string
}

export interface DatabaseStatus {
  database: DatabaseRecord
  tables: number
  rows: number
}

export interface ProbeColumn {
  id: number
  name: string
  mysql_data_type: string
  mysql_column_type: string
  pintail_type: string | Record<string, number>
  nullable: boolean
}

export interface ProbeTable {
  name: string
  engine: string | null
  estimated_rows: number | null
  columns: ProbeColumn[]
  key: {
    mode: 'primary' | 'unique' | 'append_row_id'
    index_name: string | null
    columns: string[]
  }
  unique_keys: string[][]
  requires_reconciliation: boolean
  warnings: string[]
}

export interface ProbeReport {
  database: string
  server: {
    version: string
    version_comment: string
    flavor: 'mysql' | 'maria_db'
  }
  capabilities: {
    log_bin: boolean
    row_binlog: boolean
    full_row_image: boolean
    full_row_metadata: boolean
    replication_grants: boolean
    global_read_lock: boolean
    gtid_available: boolean
    recommended_mode: 'cdc' | 'polling'
    reasons: string[]
  }
  tables: ProbeTable[]
  warnings: string[]
}

export interface TableSummary {
  name: string
  state: string
  rows: number
  schema_version: number
  last_error: string | null
  /** A source foreign key cascades into this table. MySQL performs cascades
   *  inside InnoDB without writing row events, so they are invisible to CDC
   *  and repaired by scheduled reconciliation instead of arriving in seconds. */
  cascade_reconciled: boolean
}

export interface SnapshotStatus {
  database_id: string
  state: string
  effective_mode: string | null
  tables: Array<{
    name: string
    state: string
    rows: number
    completed_chunks: number
    total_chunks: number
    last_error: string | null
  }>
}

export interface QueryResponse {
  fields: Array<{
    name: string
    data_type: string | Record<string, number> | null
    nullable: boolean
  }>
  rows: unknown[][]
  stats: {
    duration_ms: number
    rows: number
    batches: number
    segments_read: number
    segments_pruned: number
    blocks_read: number
    blocks_pruned: number
    blocks_decoded: number
  }
  truncated: boolean
}

export interface ActivityRecord {
  id: string
  database_id: string
  table: string | null
  kind: string
  status: string
  rows: number
  bytes: number
  duration_ms: number | null
  error: string | null
  started_at: string
}

export interface DlqRecord {
  id: string
  database_id: string
  table: string | null
  event: unknown
  error: string
  created_at: string
}

export interface BackupConfig {
  configured: boolean
  bucket: string
  prefix: string
  endpoint: string | null
  region: string
  schedule_minutes: number
  enabled: boolean
  credentials_configured: boolean
  updated_at: string
}

export interface BackupRecord {
  id: string
  database_id: string
  kind: 'full' | 'incremental'
  parent_id: string | null
  object_prefix: string
  status: 'running' | 'completed' | 'error'
  bytes: number
  object_count: number
  error: string | null
  started_at: string
  completed_at: string | null
}

export interface ApiKeyRecord {
  id: string
  database_id: string
  name: string
  enabled: boolean
  scopes: string[]
  expires_at: string | null
  last_used_at: string | null
  created_at: string
  secret?: string
}
