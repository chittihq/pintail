/// Bug classes that matter to Pintail, and how a fix commit in each upstream
/// repository is assigned to them from the paths it touches.
///
/// Classification is by path, not by message text: a commit's diff says which
/// subsystem it repaired far more reliably than its summary line, and paths are
/// stable enough across decades to give a trend. A commit touching several
/// classes counts once in each.

export type Project = 'mysql' | 'mariadb' | 'clickhouse'

export type BugClass = {
  id: string
  title: string
  /// What goes wrong, in Pintail's terms.
  description: string
  /// The Pintail gates that cover the class today; empty means uncovered.
  gates: string[]
  /// Whether a bug of this class can occur in Pintail at all.
  relevance: 'core' | 'adjacent' | 'out-of-scope'
  rules: Partial<Record<Project, RegExp[]>>
}

const mysqlish = (...patterns: RegExp[]) => ({ mysql: patterns, mariadb: patterns })

export const CLASSES: BugClass[] = [
  {
    id: 'decimal',
    title: 'Exact numeric arithmetic',
    description: 'DECIMAL precision, scale, rounding, overflow and conversion to and from text or floats.',
    gates: ['oracle', 'mtr'],
    relevance: 'core',
    rules: {
      ...mysqlish(/(^|\/)(strings|mysys)\/decimal\.c/, /sql\/my_decimal\./, /mysql-test\/(t|main)\/(type_decimal|type_newdecimal|func_math|round|decimal)/),
      clickhouse: [/src\/DataTypes\/DataTypeDecimal/, /src\/Core\/DecimalFunctions/, /src\/Functions\/.*[Rr]ound/, /src\/Common\/(arithmeticOverflow|FieldVisitor)/],
    },
  },
  {
    id: 'float-format',
    title: 'Floating-point formatting and parsing',
    description: 'Shortest round-trip printing, exponent forms and float/text conversion.',
    gates: ['oracle', 'mtr'],
    relevance: 'core',
    rules: {
      ...mysqlish(/strings\/dtoa\.c/, /mysql-test\/(t|main)\/type_float/),
      clickhouse: [/src\/IO\/(readFloatText|WriteHelpers|DoubleConverter)/, /base\/base\/.*(dtoa|ryu)/],
    },
  },
  {
    id: 'collation',
    title: 'Character sets and collations',
    description: 'Equality, ordering, grouping, LIKE and padding under a collation; charset conversion.',
    gates: ['oracle', 'mtr'],
    relevance: 'core',
    rules: {
      ...mysqlish(/strings\/ctype[-_]/, /mysys\/charset/, /sql\/sql_locale/, /mysql-test\/(t|main)\/ctype_/, /strings\/uca/),
      clickhouse: [/src\/Functions\/.*(Collat|UTF8|lowerUpper)/, /src\/Columns\/Collator/],
    },
  },
  {
    id: 'temporal',
    title: 'Dates, times and time zones',
    description: 'Temporal parsing, zero and negative values, fractional seconds, time zone conversion, interval arithmetic.',
    gates: ['oracle', 'mtr', 'e2e'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql-common\/my_time\.c/, /mysys\/my_time/, /sql\/tztime\./, /sql\/item_timefunc\./, /mysql-test\/(t|main)\/(type_date|type_time|type_datetime|type_timestamp|func_time|func_date|timezone)/),
      clickhouse: [/src\/Common\/DateLUT/, /src\/Functions\/(toStartOf|DateTime|dateDiff|formatDateTime|parseDateTime|FunctionsTimeWindow)/, /src\/DataTypes\/DataTypeDate/],
    },
  },
  {
    id: 'json',
    title: 'JSON values',
    description: 'JSON parsing, path evaluation, comparison and serialization.',
    gates: ['oracle', 'mtr'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/(json_|item_json_func)/, /mysql-test\/(t|main)\/(json|func_json)/),
      clickhouse: [/src\/Functions\/.*JSON/, /src\/DataTypes\/.*(Object|JSON)/],
    },
  },
  {
    id: 'type-coercion',
    title: 'Type inference and comparison coercion',
    description: 'Result types of expressions, mixed-type comparison, CAST, IN/BETWEEN comparison types.',
    gates: ['oracle', 'mtr'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/item_cmpfunc\./, /sql\/item\.cc/, /sql\/field_conv/, /sql\/item_func\.cc/, /mysql-test\/(t|main)\/(func_in|func_equal|cast)/),
      clickhouse: [/src\/DataTypes\/getLeastSupertype/, /src\/Functions\/(FunctionsComparison|castType|FunctionsConversion)/],
    },
  },
  {
    id: 'string-functions',
    title: 'String and scalar functions',
    description: 'Scalar function semantics at edges: NULL, empty, binary, multi-byte, overflow.',
    gates: ['oracle', 'mtr'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/item_strfunc\./, /mysql-test\/(t|main)\/func_(str|concat|like|regexp|gconcat)/, /sql\/(item_regexp|regexp\/)/),
      clickhouse: [/src\/Functions\/.*(String|Substring|concat|replace|position|like|Regexp|regexp)/],
    },
  },
  {
    id: 'aggregation',
    title: 'Aggregation and grouping',
    description: 'GROUP BY semantics, aggregate state, DISTINCT, ROLLUP, HAVING, empty and NULL groups.',
    gates: ['oracle', 'mtr', 'live-replication-queries'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/item_sum\./, /sql\/(sql_group|temp_table_param|sql_tmp_table)/, /mysql-test\/(t|main)\/(group_by|func_group|distinct|having|olap)/),
      clickhouse: [/src\/AggregateFunctions\//, /src\/Interpreters\/Aggregator/, /src\/Processors\/Transforms\/Aggregating/],
    },
  },
  {
    id: 'window',
    title: 'Window functions',
    description: 'Frame bounds, peers, partition boundaries and ordering inside windows.',
    gates: ['oracle', 'mtr'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/window/, /sql\/item_window/, /mysql-test\/(t|main)\/(window|win)/),
      clickhouse: [/src\/Processors\/Transforms\/WindowTransform/, /src\/Interpreters\/WindowDescription/],
    },
  },
  {
    id: 'planner-wrong-result',
    title: 'Planner and rewrite wrong results',
    description: 'Subquery decorrelation, derived-table merging, predicate pushdown, join reordering and outer-join simplification returning different rows.',
    gates: ['oracle', 'mtr', 'plan-quality'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/(sql_optimizer|sql_select|sql_resolver|opt_|sql_planner|item_subselect|sql_derived|sql_union|join_optimizer\/|opt_subselect|table_elimination|sql_cte)/, /mysql-test\/(t|main)\/(subselect|subquery|derived|join|cte|union|select|order_by|limit|opt_)/),
      clickhouse: [/src\/(Analyzer|Planner)\//, /src\/Interpreters\/(TreeRewriter|InterpreterSelect|ExpressionAnalyzer|Optimize|PredicateExpressionsOptimizer|CrossToInner|JoinToSubquery)/, /src\/Processors\/QueryPlan\/Optimizations/],
    },
  },
  {
    id: 'join-execution',
    title: 'Join execution',
    description: 'Hash, merge and nested-loop join operators: NULL keys, spilling, outer padding, duplicates.',
    gates: ['oracle', 'mtr', 'join-spill'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/(hash_join|sql_join_buffer|sql_join_cache|sql_executor|iterators\/.*join|composite_iterators)/),
      clickhouse: [/src\/Interpreters\/(HashJoin|MergeJoin|FullSortingMergeJoin|GraceHashJoin|ConcurrentHashJoin|JoinSwitcher|joinDispatch)/, /src\/Processors\/Transforms\/.*Join/],
    },
  },
  {
    id: 'sort-limit',
    title: 'Sorting, top-k and spilling',
    description: 'Ordering stability, top-k, external sort, memory limits during sort.',
    gates: ['oracle', 'sort-spill', 'sort-determinism'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/(filesort|sql_sort|records|bounded_queue|priority_queue)/),
      clickhouse: [/src\/Processors\/Transforms\/(MergeSorting|PartialSorting|FinishSorting|LimitsCheckingTransform)/, /src\/Processors\/Merges\//, /src\/Interpreters\/sortBlock/],
    },
  },
  {
    id: 'binlog-events',
    title: 'Binary log events and decoding',
    description: 'Row event decoding, type encodings in row images, metadata, compressed payloads, GTID and rotation events.',
    gates: ['cdc-matrix', 'cdc-integration', 'e2e', 'fuzz-binlog'],
    relevance: 'core',
    rules: {
      ...mysqlish(/(libbinlogevents|libs\/mysql\/binlog\/event)\//, /sql\/(log_event|rpl_record|binlog)/, /mysql-test\/suite\/binlog/),
      clickhouse: [/src\/Core\/MySQL\/MySQLReplication/, /src\/Core\/MySQL\/.*Binlog/],
    },
  },
  {
    id: 'replication-apply',
    title: 'Replication apply and position tracking',
    description: 'Applying changes in order, restart and resume positions, GTID sets, skipped or duplicated transactions, purged logs.',
    gates: ['cdc-sim', 'cdc-matrix', 'mtr-replica', 'cdc-integration', 'e2e', 'recovery'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/rpl_(gtid|replica|slave|applier|rli|mi|source|master|trx|handler|channel)/, /sql\/(rpl_gtid|log\.cc|gtid)/, /mysql-test\/suite\/(rpl|rpl_gtid|rpl_nogtid)\//),
      clickhouse: [/src\/Databases\/MySQL\/(MaterializedMySQL|MaterializeMetadata|DatabaseMaterializedMySQL)/, /src\/Storages\/StorageMaterializedMySQL/],
    },
  },
  {
    id: 'ddl-under-replication',
    title: 'Schema changes under a live mirror',
    description: 'ALTER, RENAME, TRUNCATE and type changes arriving through the change stream; DDL parsing; row images after a schema change.',
    gates: ['cdc-sim', 'cdc-matrix', 'mtr-replica', 'migrations', 'e2e', 'recovery'],
    relevance: 'core',
    rules: {
      mysql: [/sql\/(sql_table|sql_alter|sql_rename|sql_truncate|dd_table)/, /mysql-test\/suite\/rpl\/t\/.*(alter|ddl)/, /mysql-test\/(t|main)\/alter_table/],
      mariadb: [/sql\/(sql_table|sql_alter|sql_rename|sql_truncate)/, /mysql-test\/suite\/rpl\/t\/.*(alter|ddl)/, /mysql-test\/main\/alter_table/],
      clickhouse: [/src\/Databases\/MySQL\/.*(DDL|Alter)/, /src\/Parsers\/MySQL\//, /src\/Interpreters\/MySQL\//, /src\/Storages\/AlterCommands/, /src\/Storages\/MergeTree\/.*Mutat/],
    },
  },
  {
    id: 'snapshot-consistency',
    title: 'Initial copy and snapshot handoff',
    description: 'Consistent initial copy, chunked reads, the handoff position between copy and stream.',
    gates: ['snapshot-integration', 'e2e'],
    relevance: 'core',
    rules: {
      ...mysqlish(/client\/mysqldump/, /client\/(mysqlpump|dump\/)/, /mysql-test\/(t|main)\/(mysqldump|consistent_snapshot)/),
      clickhouse: [/src\/Databases\/MySQL\/MaterializedMySQLSyncThread/, /src\/Processors\/Sources\/MySQLSource/],
    },
  },
  {
    id: 'wire-protocol',
    title: 'Client protocol',
    description: 'Handshake, authentication, result metadata, prepared statements, text and binary encodings, client compatibility.',
    gates: ['wire-compat', 'bi-clients', 'fuzz-wire'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/(protocol|sql_prepare|auth\/|sql_connect)/, /sql-common\/client/, /libmysql\//, /mysql-test\/(t|main)\/(ps|mysql_client_test)/),
      clickhouse: [/src\/Server\/MySQLHandler/, /src\/Core\/MySQL\/(PacketsProtocolText|PacketsConnection|PacketsGeneric|Authentication|MySQLClient)/, /src\/Formats\/.*MySQL/, /src\/Processors\/Formats\/Impl\/MySQL/],
    },
  },
  {
    id: 'columnar-merge',
    title: 'Immutable parts, merges and versioned deduplication',
    description: 'Background merges of sorted runs, version-resolving reads, tombstones and overlap between runs.',
    gates: ['recovery-sequences', 'live-replication-queries'],
    relevance: 'core',
    rules: {
      clickhouse: [/src\/Storages\/MergeTree\/(MergeTask|MergeTreeDataMergerMutator|Merge|IMergeTreeDataPart|MergeTreeData\.|PartsSplitter|MergeTreeSelectProcessor|MergeTreeDataSelectExecutor|ReplacingSorted|MergeTreeRangeReader|MergeTreeReadPool)/, /src\/Processors\/Merges\/Algorithms\/(Replacing|VersionedCollapsing|Collapsing)/],
    },
  },
  {
    id: 'index-pruning',
    title: 'Skip indexes and pruning',
    description: 'Min/max statistics, key-range analysis and pruning that skips data it should have read.',
    gates: ['oracle', 'date-prune'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/(opt_range|range_optimizer\/)/),
      clickhouse: [/src\/Storages\/MergeTree\/(KeyCondition|MergeTreeIndex|PartitionPruner|MergeTreeWhereOptimizer|RPNBuilder)/, /src\/Storages\/MergeTree\/.*Skip/],
    },
  },
  {
    id: 'storage-format',
    title: 'On-disk format, encodings and compression',
    description: 'Encoders, codecs, checksums, file layout and format evolution.',
    gates: ['disk-faults', 'fuzz-storage', 'crash-fuzz'],
    relevance: 'core',
    rules: {
      clickhouse: [/src\/Compression\//, /src\/Storages\/MergeTree\/(MergeTreeDataPartWriter|MergeTreeReaderCompact|MergeTreeReaderWide|MergeTreeMarks|Checksum|DataPartStorage)/, /src\/DataTypes\/Serializations\//],
    },
  },
  {
    id: 'crash-recovery',
    title: 'Crash recovery and durability',
    description: 'Write-ahead logging, torn writes, fsync, partial files, restart with inconsistent metadata.',
    gates: ['disk-faults', 'crash-fuzz', 'recovery-sequences', 'recovery'],
    relevance: 'core',
    rules: {
      mysql: [/storage\/innobase\/(log|recv|fil\/|os\/os0file|buf\/buf0dblwr)/],
      mariadb: [/storage\/innobase\/(log|recv|fil\/|os\/os0file|buf\/buf0dblwr)/, /storage\/maria\/ma_(recovery|loghandler)/],
      clickhouse: [/src\/Disks\//, /src\/IO\/(WriteBufferFromFile|fsync|ReadBufferFromFile)/, /src\/Storages\/MergeTree\/(MergeTreeWriteAheadLog|MergeTreeDataPartCompact|checkDataPart|MergeTreePartsMover)/, /src\/Common\/(FailPoint|filesystemHelpers)/],
    },
  },
  {
    id: 'concurrency',
    title: 'Concurrency between readers, writers and background work',
    description: 'Races between queries, ingestion, merges, schema changes and shutdown; lock ordering; use-after-free on shared state.',
    gates: ['live-replication-queries', 'load', 'memsoak'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/(mdl|sql_base|table_cache|lock)\./),
      clickhouse: [/src\/Storages\/MergeTree\/(BackgroundJobsAssignee|MergeTreeBackgroundExecutor|MergeList)/, /src\/Common\/(ThreadPool|RWLock|SharedMutex)/, /src\/Interpreters\/(ProcessList|DDLWorker|Context\.cpp)/],
    },
  },
  {
    id: 'resource-limits',
    title: 'Memory and resource limits',
    description: 'Accounting, spilling, cancellation and out-of-memory behaviour under limits.',
    gates: ['agg-spill', 'budget-spill', 'memsoak'],
    relevance: 'core',
    rules: {
      ...mysqlish(/sql\/(sql_tmp_table|temptable)/, /storage\/temptable\//),
      clickhouse: [/src\/Common\/(MemoryTracker|OvercommitTracker|CurrentMemoryTracker)/, /src\/Interpreters\/TemporaryDataOnDisk/, /src\/QueryPipeline\/.*(Cancel|Limits)/],
    },
  },
  {
    id: 'information-schema',
    title: 'Catalog and metadata views',
    description: 'information_schema contents, SHOW commands and metadata clients depend on.',
    gates: ['wire-compat', 'bi-clients'],
    relevance: 'adjacent',
    rules: {
      ...mysqlish(/sql\/(sql_show|dd\/impl\/system_views|information_schema)/, /mysql-test\/(t|main)\/(information_schema|show_)/),
      clickhouse: [/src\/Storages\/System\//, /src\/Storages\/InformationSchema/],
    },
  },
  {
    id: 'out-of-scope',
    title: 'Outside Pintail',
    description: 'Transactions, locking engines, replication topologies, privileges, GIS, full text, partitions, XA and clustering.',
    gates: [],
    relevance: 'out-of-scope',
    rules: {
      ...mysqlish(/storage\/(innobase|myisam|maria|ndb|rocksdb|spider|connect|mroonga|federated|archive|csv|perfschema)\//, /sql\/(gis|spatial|item_geofunc|sql_partition|partition_|xa|sql_acl|auth_)/, /plugin\//, /mysql-test\/suite\/(innodb|ndb|gis|group_replication|perfschema|parts|sys_vars|x|galera|federated)/),
      clickhouse: [/src\/Storages\/(Kafka|RabbitMQ|NATS|S3Queue|HDFS|Hive|Iceberg|DataLakes|ObjectStorage|StorageDistributed|Distributed)/, /src\/Coordination\//, /src\/Access\//, /src\/Databases\/(Replicated|DatabaseReplicated)/],
    },
  },
]

/// Which classes have a gate that GENERATES new cases against an oracle, as
/// opposed to replaying cases someone wrote. A hand-written gate catches the
/// bugs its author imagined; the generated ones are how a class stops
/// depending on that. Classes absent here are the verification backlog.
export const GENERATORS: Record<string, string> = {
  decimal: 'oracle fuzz (DECIMAL family)',
  'float-format': 'oracle fuzz (numeric family)',
  collation: 'oracle fuzz (string family, collation matrix)',
  temporal: 'oracle fuzz (temporal family)',
  json: 'oracle fuzz (JSON family)',
  'type-coercion': 'oracle fuzz (all families), kernel differential',
  'string-functions': 'oracle fuzz (string, hash and encoding families)',
  aggregation: 'oracle fuzz (grouping family), live-replication-queries',
  window: 'oracle fuzz (window family)',
  'planner-wrong-result': 'oracle fuzz (join, subquery and set families), metamorphic pack',
  'join-execution': 'oracle fuzz (join families)',
  'sort-limit': 'oracle fuzz (ordered answers)',
  'binlog-events': 'cdc-matrix (source versions and binlog settings), fuzz-binlog',
  'replication-apply': 'cdc-sim (crashes and replay), cdc-matrix, mtr replica mode',
  'ddl-under-replication': 'cdc-sim (added columns, truncates), cdc-matrix, mtr replica mode',
  'snapshot-consistency': 'cdc-matrix (restarts mid-round)',
  'columnar-merge': 'recovery-sequences, live-replication-queries',
  'storage-format': 'disk-faults, fuzz-storage',
  'crash-recovery': 'disk-faults, crash-fuzz, recovery-sequences, cdc-sim',
  'wire-protocol': 'fuzz-wire (packet parsing only)',
}

/// Test files a fix commit may add or change, per project.
export const TEST_PATHS: Record<Project, RegExp> = {
  mysql: /^mysql-test\/(t|r|suite\/[^/]+\/(t|r))\/[^/]+\.(test|result|inc)$/,
  mariadb: /^mysql-test\/(main|suite\/[^/]+(\/[^/]+)?)\/[^/]+\.(test|result|inc)$/,
  clickhouse: /^tests\/queries\/0_stateless\/[^/]+\.(sql|sh|reference|j2|python)$/,
}

/// How each project names the bug a commit fixes.
export const BUG_IDS: Record<Project, RegExp> = {
  mysql: /\bBug\s*#\s*(\d{4,9})\b/gi,
  mariadb: /\bMDEV-(\d{2,6})\b/g,
  clickhouse: /(?:Merge pull request|\(#)\s*#?(\d{3,6})/g,
}

export function classify(project: Project, paths: string[]): string[] {
  const hits = new Set<string>()
  for (const cls of CLASSES) {
    const rules = cls.rules[project]
    if (!rules) continue
    if (paths.some((path) => rules.some((rule) => rule.test(path)))) hits.add(cls.id)
  }
  // A commit that touches Pintail-relevant code is not also out of scope just
  // because the same change adjusted a storage engine or a plugin.
  if (hits.size > 1) hits.delete('out-of-scope')
  return [...hits].sort()
}

export function bugIds(project: Project, message: string): string[] {
  return [...new Set([...message.matchAll(BUG_IDS[project])].map((m) => m[1]))]
}

export function testPaths(project: Project, paths: string[]): string[] {
  return paths.filter((path) => TEST_PATHS[project].test(path))
}
