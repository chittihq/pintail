# The next 50 experiments: a biomimetic research program

Status: **screening complete (34 rejected, 16 advanced)**. All fifty experiments now have
code, deterministic output checks, recorded results, and a screening verdict. The sixteen
advanced items remain simulation, model, or isolated-kernel evidence; none is validated for
engine adoption until its stated real PTSEG or integration follow-up passes. A biological
analogy is a source of mechanisms, not evidence that the mechanism works in a database.

These are e30 through e79. They deliberately exclude the mechanisms already isolated in
e01-e29: filter representations, aggregation-table shapes, guarded Top-K heaps, join-table
layouts, sweep-classified merge-on-read, fixed-size morsels, normalized merge keys,
typed execution, fixed block SMAs, condition caches, FastLanes, and per-block compression
selection. Where an experiment touches the same operator, it asks a different question:
online control, reorganization, resilience, or workload drift.

## Evidence contract

Every experiment must have:

1. A static or current-policy baseline and at least one biomimetic candidate.
2. Deterministic inputs, randomized variant order, warmup, median-of-seven timing, and
   per-run stable output checks. Exact result comparison replaces XOR-only checks whenever
   the output is tractable.
3. At least three workload shapes, including a hostile or regime-change shape. Adaptive
   policies must pay their learning, metadata, and reorganization costs.
4. A primary metric and a safety/resource guardrail decided before measurement. A speed
   win that breaks the guardrail is a loss.
5. Local Apple Silicon evidence first. Candidates that win by at least 15% then run on
   the pinned Linux/x86 target. Cross-machine disagreement means “specialize or reject,”
   never a universal win.
6. A stated evidence tier: **simulation**, **isolated kernel**, **real PTSEG path**, or
   **end-to-end engine**. Simulation and kernel wins can justify the next prototype, not
   engine adoption. Every adopted idea must eventually pass the production workload and
   Pintail's correctness gates.

Unless an item states a stricter gate, it advances when it improves the primary metric by
at least 15% on both machines, adds no incorrect answers, stays inside its resource
guardrail, and does not make the hostile workload more than 5% worse.

## A. Self-organizing data topology

### e30 — Physarum access network

**Biology.** Slime-mold tubes thicken under useful flow and decay when unused, producing
efficient, fault-tolerant transport networks without central planning.
**Mechanism.** Maintain decaying query-flow weights between projected columns and segment
ranges. At compaction, use those weights to choose a small number of co-located column
bundles while retaining the canonical PTSEG representation.
**Trial.** Replay stable, shifting, and adversarial projection traces over real files;
compare canonical column chunks, frequency-only bundling, and flow-with-decay bundling.
**Gate.** At least 20% lower cold p95 bytes/requests-to-answer after charging rewrite cost,
with at most 8% extra storage and recovery from a phase shift within 200 queries.

### e31 — Ant-pheromone join paths

**Biology.** Ants reinforce successful trails and volatile pheromone lets obsolete trails
fade.
**Mechanism.** For each normalized multi-join template, reinforce join edges by work saved
per produced row; evaporate evidence by query count and keep probabilistic exploration.
**Trial.** Compare Pintail's static rule order, an exact dynamic-programming oracle, UCB,
and pheromone routing across correlated data with two abrupt distribution shifts.
**Gate.** Within 10% of oracle cumulative runtime, at least 25% below the static policy,
and no single-query regret above 2x after warmup.

### e32 — Bone-remodelled sparse indexes

**Biology.** Trabecular bone adds structure along persistent stress and resorbs structure
under disuse.
**Mechanism.** Spend a fixed sparse-index byte budget unevenly: add pivots in key ranges
with high seek stress and merge pivots in cold ranges, using hysteresis to stop oscillation.
**Trial.** Point/range traces over uniform, Zipf, moving-hotspot, and scan-heavy workloads;
compare fixed 16K pivots, frequency-only splitting, and stress/remodelling.
**Gate.** At least 20% lower p95 rows decoded under the same metadata bytes, less than 2%
write amplification, and no scan regression above 3%.

### e33 — Leaf-venation metadata

**Biology.** Leaf veins combine low-cost local capillaries with redundant higher-order
transport paths.
**Mechanism.** Add a budgeted hierarchy of overlapping min/max summaries only where child
blocks show spatial correlation; a query may jump at coarse, medium, or leaf resolution.
**Trial.** Compare fixed block zone maps, a complete segment tree, and selective venation
on clustered, partially clustered, and scattered columns.
**Gate.** At least 30% fewer metadata probes plus data blocks read than fixed zone maps at
no more than 0.5% segment-size overhead; scattered data must stay within 3%.

### e34 — Root-foraging micro-indexes

**Biology.** Roots proliferate locally in nutrient-rich patches while whole-plant signals
cap total growth.
**Mechanism.** Allocate a global auxiliary-index budget to range-local “rootlets” based on
observed benefit per byte. Rootlets can be tiny sorted value-to-row lists or abandon a
depleted patch.
**Trial.** Mixed equality/range workloads with stationary, migrating, and decoy hotspots;
compare no index, one global secondary index, frequency allocation, and local+global control.
**Gate.** At least 25% lower cumulative query time after build cost, bounded index bytes,
and retirement of decoy indexes within two benefit half-lives.

## B. Immune-style recognition and containment

### e35 — Clonal kernel repertoire

**Biology.** Immune repertoires keep diverse receptors, clone effective ones, and retain
memory cells rather than betting on one universal detector.
**Mechanism.** Keep a small repertoire of safe operator kernels. Context keys are cheap
block facts; successful kernels receive more traffic while rare contexts retain an
exploration reserve.
**Trial.** Filter+aggregate and hash-probe kernels across selectivity, null density,
cardinality, and architecture; compare one global winner, hand thresholds, epsilon-greedy,
and clonal selection with aging.
**Gate.** Within 5% of a hindsight per-context oracle and at least 15% below the global
winner, with dispatch below 1% of runtime and no context starved of validation traffic.

### e36 — Negative-selection CDC sentinels

**Biology.** Immune negative selection removes receptors that attack “self,” leaving a
diverse set that recognizes unfamiliar non-self patterns.
**Mechanism.** Generate compact detectors that never match a training envelope of healthy
transaction features but cover gaps outside it; quarantine only when detector consensus
and an invariant signal agree.
**Trial.** Healthy CDC traces plus injected truncation, version regression, schema mismatch,
row-count burst, and adversarial-but-valid transactions; compare fixed thresholds,
Mahalanobis distance, and negative-selection ensembles.
**Gate.** At least 95% injected-fault recall, below 0.1% false quarantine, under 1% ingest
CPU, and never replace checksum/schema validation.

### e37 — Immune-memory plan recall

**Biology.** Memory cells answer recurring threats quickly but decay when no longer useful.
**Mechanism.** Cache not merely a plan but the parameter/data-shape envelope in which it
won. A fast affinity test decides recall; low-affinity requests explore another plan.
**Trial.** Parameter-sensitive templates with skew and concept drift; compare one cached
plan, LRU by SQL text, parameter buckets, and affinity+decay memory.
**Gate.** At least 25% lower cumulative runtime than text-LRU, below 2% planning overhead,
and recovery from the worst plan after a shift in at most 20 executions.

### e38 — Fever-mode overload control

**Biology.** Fever temporarily changes the body's operating point to prioritize survival
over normal peak efficiency.
**Mechanism.** Under sustained queue pressure, enter a hysteretic mode that caps scan
parallelism, disables speculative work, shrinks prefetch, and reserves capacity for CDC.
**Trial.** Concurrent analytical bursts plus fixed-rate CDC; compare unconstrained,
fixed conservative limits, and fever-mode control under mild and extreme overload.
**Gate.** Zero CDC buffer overflow, at least 30% lower query p99 than unconstrained overload,
less than 5% throughput loss outside overload, and no rapid mode flapping.

### e39 — Granule quarantine membranes

**Biology.** Inflammation contains damage locally instead of shutting down the organism.
**Mechanism.** Persist verified bad-granule intervals and prevent them from contaminating
unrelated scans. Queries provably disjoint from quarantine proceed; overlapping queries
fail loudly with the same corruption semantics.
**Trial.** Corrupt selected PTSEG blocks and compare repeated whole-segment discovery with
range-aware quarantine across point, disjoint-range, and overlapping-range queries.
**Gate.** 100% corruption detection, zero partial/silent answers, at least 80% less repeated
verification work, and unaffected ranges retain at least 95% availability.

## C. Neural learning and attention

### e40 — Hippocampal trace replay

**Biology.** Rest replay preferentially consolidates weak or important memories rather
than merely replaying the most frequent event.
**Mechanism.** During maintenance windows, replay a bounded sample of costly/mispredicted
query traces to choose statistics, indexes, or compiled metadata to build next.
**Trial.** Compare frequency, total-cost, worst-error, and weak-trace-prioritized replay on
periodic and shifting workloads, charging all replay and build work.
**Gate.** At least 20% lower next-window p95 and cumulative work than frequency replay,
within a 1% maintenance CPU budget, without evicting an item whose saved work exceeds cost.

### e41 — Synaptic plan-cache pruning

**Biology.** Synapses strengthen with useful co-activation and weak connections are pruned
to restore capacity and signal-to-noise.
**Mechanism.** Represent cached subplans/intermediates as a dependency graph. Reinforce an
edge by measured work saved, globally downscale periodically, then remove weak islands.
**Trial.** Dashboard trace with overlapping subplans and one-off queries; compare entry-LRU,
LFU, GreedyDual-size, and graph reinforcement/pruning.
**Gate.** At least 20% greater work saved per cache byte, under 2% bookkeeping time, and
resident bytes never exceed the hard cap.

### e42 — Predictive-coding columns

**Biology.** Predictive coding transmits surprise—the residual from expectation—rather
than retransmitting predictable sensory input.
**Mechanism.** For correlated numeric columns, choose a simple per-block predictor from
already-required columns and encode exact residuals plus exceptions; abandon prediction
when sampled residual entropy is high.
**Trial.** Timestamps, monotonic IDs, price×quantity, seasonal series, and random controls;
compare PTSEG FOR/delta, one-dimensional residuals, and two-feature predictors.
**Gate.** At least 20% smaller and 15% faster projected decode on two correlated shapes,
bit-exact reconstruction, under 1% expansion on random data, and predictor metadata <0.2%.

### e43 — Lateral-inhibition predicates

**Biology.** Lateral inhibition amplifies informative contrast and suppresses redundant
neighbor activity.
**Mechanism.** Score each conjunct by rows eliminated per nanosecond and discount predicates
whose rejected rows strongly overlap another predicate. Reorder independently per block.
**Trial.** Independent, correlated, anti-correlated, expensive, and drifting conjuncts;
compare SQL order, static global selectivity/cost, greedy elimination, and overlap-discounted
per-block order.
**Gate.** At least 20% lower predicate CPU than the best static order, exact masks, sampling
under 2%, and no shape over 5% slower.

### e44 — Foveated Top-K materialization

**Biology.** The retina spends high resolution near likely targets and preserves cheap
peripheral awareness elsewhere.
**Mechanism.** Decode score columns everywhere, cheap bound columns for a wider candidate
ring, and expensive payloads only inside the current safe Top-K frontier. All pruning uses
proof-carrying bounds, never a probability.
**Trial.** Narrow/wide payloads, correlated/uncorrelated sort keys, clustered/scattered
leaders; compare eager rows, score-only late materialization, and two-ring foveation.
**Gate.** At least 30% fewer decoded payload bytes than score-only late materialization,
exact ordered results, and less than 5% overhead when correlation is absent.

### e45 — Homeostatic cardinality feedback

**Biology.** Neural homeostasis adjusts local gain while keeping total activity within a
stable range, preventing both saturation and runaway correction.
**Mechanism.** Feed actual operator cardinalities into bounded multiplicative corrections;
normalize total correction strength and decay it toward catalog statistics after drift.
**Trial.** Correlated predicates and multi-join workloads with abrupt reversals; compare
static HLL/independence, unbounded feedback, exponentially weighted correction, and
homeostatic normalization.
**Gate.** At least 50% lower median q-error and 20% lower execution time than static,
no correction beyond a declared bound, and convergence within 10 observations after shift.

## D. Collective behavior and distributed local rules

### e46 — Quorum-sensing compaction

**Biology.** Bacteria delay collective action until local signal concentration indicates
that enough peers are present for the action to pay.
**Mechanism.** Each segment emits signals for overlap, tombstone density, read pain, age,
and size. Compact only when a neighborhood quorum crosses a threshold and memory tokens exist.
**Trial.** Bursty CDC, scattered updates, append-only, and read-heavy traces; compare fixed
segment count, size-tier debt, global score, and local quorum.
**Gate.** At least 25% lower total read+write bytes than current scheduling, p99 scan down
20%, write amplification no higher, and compaction memory never exceeds its token budget.

### e47 — Bee-waggle morsel discovery

**Biology.** Foragers advertise high-yield patches with signal strength proportional to
quality, while scouts preserve exploration.
**Mechanism.** Workers publish observed match density and work/row for recently scanned
regions; idle workers probabilistically steal from promising regions while some remain scouts.
**Trial.** Clustered sparse matches, uniform matches, moving clusters, and costly UDFs;
compare FIFO morsels, random steal, density priority, and waggle+scouts.
**Gate.** At least 20% lower time-to-first-result and 15% lower completion time on clustered
shapes, exact results, and uniform work no more than 3% slower.

### e48 — Flocking read coalescence

**Biology.** Flocks align direction using local neighbors rather than a centralized route.
**Mechanism.** Concurrent scans expose their next few immutable block reads. A local rule
nudges compatible scans toward a common read frontier so one decoded block serves several.
**Trial.** Identical, overlapping, diverging, and latency-sensitive concurrent scans on
cold real files; compare independent prefetch, global cooperative scan, and bounded flocking.
**Gate.** At least 30% fewer physical bytes/read calls and 20% lower median latency for
overlap, while non-overlap and short-query p95 regress less than 5%.

### e49 — Fish-school concurrency control

**Biology.** Schooling uses separation, alignment, and cohesion rules to avoid collisions
while moving as a group.
**Mechanism.** Each query adjusts runnable tasks from local queue delay (separation), system
throughput trend (alignment), and fair-share distance (cohesion), within hard global bounds.
**Trial.** Mixed short/long, memory/CPU-bound, and arrival-burst traces; compare fixed pool,
per-query equal slots, shortest-job-first, and schooling control.
**Gate.** At least 20% lower p95 slowdown with throughput within 5% of best baseline, no
starvation, and Jain fairness at least 0.9.

### e50 — Termite-mound maintenance ventilation

**Biology.** Termite mounds regulate gas and heat through passive feedback between local
conditions and channels.
**Mechanism.** Treat query latency, CDC lag, compaction debt, and memory pressure as four
sensors controlling a slowly varying maintenance aperture instead of independent thresholds.
**Trial.** Periodic load, sudden spikes, and noisy shared-host capacity; compare fixed
maintenance fraction, independent PID loops, and coupled ventilation with hysteresis.
**Gate.** At least 30% lower area-under-debt without p99 query or CDC SLO violation, fewer
than two control reversals per minute, and stable response under 10% sensor noise.

### e51 — Mycelial decoded-block exchange

**Biology.** Fungal networks redistribute resources through shared hyphal paths according
to local source/sink demand.
**Mechanism.** Queries publish decoded immutable blocks into a size-bounded exchange;
retention depends on downstream demand, decode cost, and transfer distance, not recency alone.
**Trial.** Concurrent dashboard scans with partial projections and phase shifts; compare no
sharing, LRU blocks, frequency blocks, and source/sink value flow.
**Gate.** At least 25% lower total decode CPU and 15% lower p95 at the same memory cap, no
mutable/WAL block sharing, and phase-shift recovery within one cache turnover.

### e52 — Spider-web join fibers

**Biology.** A web places cheap structural fibers broadly and routes detailed vibration
only along paths touched by prey.
**Mechanism.** Persist tiny per-segment join fingerprints for repeatedly traversed FK/PK
edges; a match narrows candidate blocks before building the exact join structure.
**Trial.** Star, chain, sparse-match, and no-FK joins; compare raw hash join, ordinary Bloom
pushdown, static fingerprints, and demand-grown fibers.
**Gate.** At least 25% lower join input rows than Bloom alone, exact output, metadata under
2% of indexed columns, and build cost repaid within ten matching queries.

### e53 — Coral-accretion partial views

**Biology.** Coral structures grow incrementally from many small deposits while retaining
the history and boundaries of each generation.
**Mechanism.** Build query-derived aggregate partials per immutable segment only after
repeated demand. Compose lineage-compatible partials and re-evaluate only overlapped dirty
ranges; this is not e18's fixed block SMA.
**Trial.** Stable and changing GROUP BY templates under inserts, updates, deletes, and
schema generation changes; compare full scan, fixed SMA, eager MV, and lazy segment accretion.
**Gate.** Exact snapshot results, at least 10x on clean repeated queries and 2x at 10% overlap,
storage under 5%, and all build/invalidations charged.

### e54 — Ant-trail tombstone compaction

**Biology.** Evaporating trails retain recent collective history without preserving it
forever.
**Mechanism.** Maintain a decaying spatial heat field of versions/tombstones and compact
connected hot intervals rather than whole size-tier candidates.
**Trial.** Recent-hot, moving-hotspot, scattered, and append-only CDC; compare size-tier,
overlap count, non-decaying heat, and evaporating trails.
**Gate.** At least 25% lower merge-on-read overlap work and 20% lower write amplification
than size-tier on hotspot traces; append-only extra work below 1%.

## E. Metabolism and resource homeostasis

### e55 — Stomatal prefetch gates

**Biology.** Stomata balance carbon intake against water loss by continuously adjusting
aperture.
**Mechanism.** Adjust per-scan prefetch depth from useful-byte gain versus wasted-read and
memory cost, with fast closure under pressure and slow reopening.
**Trial.** Sequential, aggressively pruned, alternating, and concurrent cold scans;
compare no prefetch, fixed depths, one-way adaptive depth, and stomatal hysteresis.
**Gate.** Within 5% of the best fixed depth for every shape, at least 20% better cumulative
latency across shifts, wasted bytes down 30%, and fixed memory ceiling respected.

### e56 — Vascular memory flow

**Biology.** Vascular networks remodel conductance toward persistent flow while global
pressure and volume constraints prevent one vessel consuming the system.
**Mechanism.** Operators bid for memory by measured marginal work saved per byte. A
pressure solver allocates tokens, slowly thickening useful paths and shrinking idle ones.
**Trial.** Concurrent hash joins, aggregates, sorts, and scans with changing utility curves;
compare equal share, first-come, static operator weights, and conductance adaptation.
**Gate.** At least 20% lower total spill I/O and 15% lower p95, hard-cap compliance, no
operator below its correctness minimum, and recovery from drift in 30 allocation epochs.

### e57 — ATP-priced physical plans

**Biology.** Cells account for work in a shared energy currency and choose pathways by
yield as well as speed.
**Mechanism.** Calibrate plan cost in allocations, decoded bytes, hash probes, I/O, and CPU
cycles, then optimize latency subject to an explicit “ATP” resource budget.
**Trial.** Equivalent plans across warm/cold and tight/loose memory regimes; compare row
count cost, wall-time regression, single-resource cost, and multi-resource currency.
**Gate.** Choose the true fastest feasible plan at least 90% of the time, resource error
under 15%, calibration overhead under 1%, and zero budget violations.

### e58 — Enzyme-kinetic batch sizing

**Biology.** Enzyme throughput saturates with substrate concentration; adding substrate
past saturation consumes capacity without proportional rate gain.
**Mechanism.** Probe adjacent vector sizes and stop growing when marginal rows/ns falls
below memory/cache cost; keep separate equilibria by operator shape.
**Trial.** Decode, filters, string functions, hash aggregation, and joins over 256..65536
rows with changing widths; compare Pintail 4096, offline best, hill-climb, and saturation fit.
**Gate.** Within 5% of offline-best runtime and 10% of its peak memory across shapes, with
adaptation cost repaid inside five batches.

### e59 — Endocrine spill coordination

**Biology.** Hormones broadcast slow systemic signals so organs coordinate rather than
reacting independently to local symptoms.
**Mechanism.** A global pressure signal changes admission, hash growth, batch size, and
spill thresholds coherently; local operators retain fast safety limits.
**Trial.** Multi-operator and multi-query spill cascades; compare independent thresholds,
one global hard threshold, PID pressure, and slow hormone+fast reflex control.
**Gate.** At least 30% fewer spill bytes and 20% lower p99, no OOM, less than 3% overhead
without pressure, and no synchronized spill storm.

### e60 — Autophagic buffer recycling

**Biology.** Autophagy selectively dismantles low-value structures and recycles their
components under nutrient stress.
**Mechanism.** Tag reusable execution buffers by shape and rebuild cost; under pressure,
recycle compatible buffers directly and digest poor matches rather than indiscriminate free.
**Trial.** Alternating scan/join/aggregate queries with allocator pressure; compare fresh
allocation, size-class pool, LRU pool, and selective recycling.
**Gate.** At least 25% fewer allocated bytes and 15% lower p95, resident slack under 10%,
contents always zeroed/initialized as required, and no long-lived fragmentation growth.

### e61 — Apoptotic runaway-query containment

**Biology.** Cells self-terminate when damage signals cross multiple independent gates,
protecting the organism from uncontrolled resource consumption.
**Mechanism.** Abort only when memory pressure, progress rate, estimated remaining work,
and SLO damage reach a consensus; prefer spill/degrade signals before termination.
**Trial.** Legitimately slow, doomed Cartesian, skew explosion, and recoverable-spill queries
amid healthy traffic; compare timeout, memory cap, single-signal progress, and consensus.
**Gate.** Stop at least 90% of doomed work before 30% resource consumption, falsely abort
under 0.1%, and improve healthy p99 by 25% under attack.

### e62 — Mitochondrial operator fission/fusion

**Biology.** Mitochondria split to isolate damage or distribute work and fuse to share
resources when fragmentation becomes costly.
**Mechanism.** Split aggregate/join state when contention or skew crosses a threshold;
merge shards when coordination overhead exceeds saved work.
**Trial.** Uniform, skewed, shifting-skew, and tiny workloads; compare always-global,
always-sharded, one-way split, and reversible fission/fusion.
**Gate.** Within 5% of the better fixed mode per phase, at least 20% better cumulative time
across shifts, exact state merge, and transition work below 5%.

### e63 — Glycogen emergency reserve

**Biology.** Organisms keep rapidly mobilizable energy stores instead of committing every
resource to current activity.
**Mechanism.** Hold a small memory reserve unavailable to normal bids, released only to
finish near-complete operators or CDC transactions and replenished before new admission.
**Trial.** Bursty memory traces with synchronized finalization and oversized CDC commits;
compare full utilization, fixed per-query padding, global reserve, and completion-aware reserve.
**Gate.** Eliminate at least 80% catastrophic spill/abort cascades with under 5% steady-state
throughput cost and no reserve capture by a new query.

### e64 — Circadian maintenance prediction

**Biology.** Circadian systems anticipate periodic demand instead of reacting only after
conditions change.
**Mechanism.** Learn a bounded seasonal arrival model and pre-position compaction, backup,
and cache downscaling before predicted low/high periods; fall back on reactive limits.
**Trial.** Daily-like periodic, drifting-period, missing-cycle, and random arrival traces;
compare fixed schedule, reactive threshold, forecast-only, and forecast+reflex.
**Gate.** At least 25% more maintenance completed inside slack with p99 improvement, no SLO
breach when the prediction is wrong, and model state below 64 KiB.

## F. Ecology, evolution, and lifecycle

### e65 — Predator-prey cache control

**Biology.** Coupled predator/prey populations avoid unconstrained growth and can track
changing resource conditions.
**Mechanism.** Cache classes grow from hits; memory-pressure predators consume entries in
proportion to low utility and population excess, with damping against oscillation.
**Trial.** Scan blocks, plans, and results under loops, scans, bursts, and phase changes;
compare LRU, LFU, ARC, and damped predator-prey control.
**Gate.** At least 15% higher work-saved hit value than ARC, hard memory compliance, phase
recovery within one cache capacity, and oscillation amplitude below 10% of capacity.

### e66 — Ecological cache niches

**Biology.** Species coexist by specializing in different resource niches rather than
competing under one universal fitness measure.
**Mechanism.** Partition cache residency dynamically among decoded blocks, plans, joins,
and results by their reuse horizon and cost curve; allow borders to move by marginal value.
**Trial.** Mixed dashboard/ad-hoc/ETL traces; compare one LRU, fixed partitions, global
GreedyDual, and adaptive niches.
**Gate.** At least 20% more total work saved than global competition, no class receives
space without positive marginal value, and bookkeeping under 2%.

### e67 — Segment ecological succession

**Biology.** Ecosystems change composition as a habitat ages; pioneer strategies differ
from mature steady-state strategies.
**Mechanism.** Let a segment progress through hot-delta, settling, mature-read, and dormant
states with different metadata, encoding, and compaction policies based on measured heat.
**Trial.** Append, recent-hot updates, stable dashboard, and archival scans; compare one
format policy, age-only tiers, heat-only tiers, and succession state machine.
**Gate.** At least 20% lower lifetime read+write CPU and 15% fewer bytes, all transition
costs charged, no format incompatibility, and at most one needless transition per segment.

### e68 — Algorithmic biodiversity reserve

**Biology.** Genetic diversity sacrifices a little immediate efficiency to preserve
resilience under environmental change.
**Mechanism.** Route 1-3% of eligible blocks to a safe runner-up kernel, tracking whether
the environment has changed; promote it only with statistically stable evidence.
**Trial.** Operator workloads with architecture, data-shape, and phase changes; compare
permanent winner, periodic benchmark, epsilon exploration, and diversity reserve.
**Gate.** Detect a true winner reversal within 50 samples, steady-state tax below 2%, false
switch below 1%, and cumulative regret 30% below periodic benchmarking.

### e69 — Invasive-template resource defense

**Biology.** Ecosystems resist invasive populations that monopolize resources and reduce
diversity.
**Mechanism.** Detect normalized templates whose marginal throughput causes disproportionate
queue, cache, or memory harm; progressively tax concurrency while preserving a minimum share.
**Trial.** One high-rate cheap query, one cache-polluting scan, and legitimate flash crowd
mixed with diverse tenants; compare FIFO, tenant quotas, template quotas, and harm feedback.
**Gate.** Improve non-invasive p99 by 30%, retain at least 90% useful total throughput,
never starve the template, and misclassify a flash crowd less than 1% of trace time.

### e70 — Forest-gap memory auctions

**Biology.** When a tree falls, species compete for the released light; the gap is filled
by the best local growth opportunities rather than a fixed successor.
**Mechanism.** When an operator releases memory, waiting operators submit marginal-work-saved
bids for the exact amount; allocation is epoch-local and revocable.
**Trial.** Staggered joins, sorts, and aggregates with non-linear memory benefits; compare
FIFO handoff, equal redistribution, static priority, and gap auctions.
**Gate.** At least 20% lower makespan and 15% lower spill bytes, auction cost under 0.5%,
hard-cap compliance, and bounded starvation age.

### e71 — Dormant auxiliary indexes

**Biology.** Seeds retain a cheap dormant form and germinate only after multiple favorable
signals, avoiding growth on a single false cue.
**Mechanism.** Serialize cold auxiliary indexes into a compact seed containing build inputs
and summary; awaken only after reuse, expected savings, and stability cues agree.
**Trial.** Seasonal, one-off, and false-start predicate workloads; compare retain-hot,
drop-rebuild, compressed dormant, and multi-cue germination.
**Gate.** At least 30% lower index memory and 20% lower cumulative rebuild work, awakening
latency within one query SLO, and false germination under 5%.

### e72 — Seasonal decoded-column migration

**Biology.** Migratory species pay relocation cost to follow recurring resource seasons.
**Mechanism.** Keep canonical compressed PTSEG plus an optional decoded hot-column mirror;
move columns in/out based on periodic demand only when predicted savings repay migration.
**Trial.** Periodic dashboards, drifting periods, random access, and wide-column controls;
compare always-compressed, always-decoded, recency, and seasonal migration.
**Gate.** At least 20% lower cumulative latency than recency under the same memory, all
decode/migration charged, and random workload within 3% of always-compressed.

### e73 — Host–microbiome intermediate exchange

**Biology.** Hosts and microbiomes exchange by-products that would otherwise be waste,
but only when the relationship benefits both sides.
**Mechanism.** A query may donate a bounded, immutable intermediate; later queries consume
or refine it. Admission requires measured net saved work after production/retention cost.
**Trial.** Related-but-nonidentical dashboard queries, adversarial one-offs, and CDC version
changes; compare result cache, exact intermediate cache, unconditional donation, and symbiosis.
**Gate.** At least 20% additional work saved beyond result caching, exact version lineage,
memory cap respected, and producer overhead below 3% when nothing reuses the donation.

### e74 — Fire-ecology cache reset

**Biology.** Disturbance can clear entrenched incumbents and allow a better-adapted community
to emerge, but excessive fire destroys productive systems.
**Mechanism.** Detect prolonged low cache productivity plus distribution shift and evict a
controlled fraction, preserving high-confidence refuges instead of waiting for LRU churn.
**Trial.** Abrupt phase shifts after long stable phases, gradual drift, and false alarms;
compare LRU, TTL, full flush, and partial reset with refuges.
**Gate.** Recover 30% faster than LRU after real shifts, steady workload miss cost under 2%,
false-reset rate under 1%, and never evict pinned/version-critical state.

## G. Regeneration and sensory systems

### e75 — Salamander parity regeneration

**Biology.** Regeneration reconstructs a missing structure from local positional information
instead of restoring the entire organism from a backup.
**Mechanism.** Add an optional XOR/erasure parity block across a small stripe of immutable
PTSEG payload blocks so one corrupt or missing block can be reconstructed locally.
**Trial.** Real encoded payloads with injected single/multiple loss; compare fail+restore,
whole-segment replica, XOR parity, and two-parity Reed-Solomon-style prototype.
**Gate.** Bit-exact repair of the promised failure count, under 8% storage and 5% scan/write
CPU overhead, at least 10x fewer recovery bytes than segment restore, and fail closed beyond
repair capacity.

### e76 — DNA-style hierarchical reconciliation

**Biology.** DNA repair locates mismatch locally through complementary structure before
rebuilding large regions.
**Mechanism.** Replace flat polling chunk checks with a persisted hierarchical digest tree;
descend only mismatching branches and reconcile exact keys at leaves.
**Trial.** Missing, extra, changed, clustered, and scattered row differences at multiple
rates; compare full key scan, flat chunk checksums, Merkle hierarchy, and adaptive fan-out.
**Gate.** Exact difference set, at least 10x fewer source rows transferred below 0.1% drift,
under 2% steady polling overhead, and no worse than flat chunks above 20% drift.

### e77 — Retinal variable-resolution granules

**Biology.** The retina allocates dense resolution to the fovea and coarse resolution to
the periphery instead of sampling all space uniformly.
**Mechanism.** Choose PTSEG granule sizes by local entropy, update overlap, and query heat;
hot/chaotic intervals get fine metadata, cold/predictable intervals get coarse blocks.
**Trial.** Clustered hot ranges, moving hotspots, uniform scans, and high-entropy data;
compare fixed 16K, static entropy, query heat, and bounded foveation.
**Gate.** At least 25% lower decoded+metadata bytes under the same metadata budget, write
throughput within 5%, and hotspot migration requires no in-place mutation.

### e78 — Olfactory Bloom receptor ensemble

**Biology.** Smells are recognized by sparse combinations across broad, overlapping
receptors rather than one receptor per odor.
**Mechanism.** Under one bit budget, maintain several differently seeded/feature-specific
blocked filters (PK prefix, tuple components, hot IN sets); combine receptor responses before
exact confirmation.
**Trial.** Point, composite-prefix, IN-list, absent-heavy, and adversarial keys; compare one
PK Bloom, partitioned Blooms, learned bit allocation, and receptor ensemble.
**Gate.** At least 30% fewer false-positive block reads at equal bits and build time within
10%; zero false negatives for every advertised query class.

### e79 — Bat-echolocation probe plans

**Biology.** Bats emit cheap probes and adapt the next signal from returning echoes before
committing to a flight path.
**Mechanism.** When plan alternatives are close or statistics uncertain, read tiny stratified
samples from candidate inputs, update selectivity/skew, then choose the full plan.
**Trial.** Correlated predicates, skewed joins, stale statistics, accurate controls, and
small queries; compare static planner, always-sample, uncertainty-triggered probe, and oracle.
**Gate.** At least 25% lower cumulative runtime on uncertain workloads including probes,
sample only when expected regret exceeds cost, accurate controls within 3%, and exact results.

## Execution order

The numbering is thematic, not priority. Implementation proceeds in vertical waves that
exercise the shared harness early:

1. **Wave 1 — adaptive metadata:** e32, e33, e34, e43, e77.
2. **Wave 2 — resource controllers:** e38, e46, e50, e55, e59, e63.
3. **Wave 3 — optimizer learning:** e31, e35, e37, e40, e45, e57, e68, e79.
4. **Wave 4 — cache and sharing:** e41, e48, e51, e65, e66, e70-e74.
5. **Wave 5 — storage and integrity:** e30, e36, e39, e42, e53, e54, e67, e75, e76, e78.
6. **Wave 6 — scheduling and execution:** e44, e47, e49, e52, e56, e58, e60-e62, e64, e69.

Wave order favors cheap falsification. A failed simulation stops an idea; a successful
simulation earns an isolated kernel; a kernel earns a real PTSEG trial; only then does it
enter the engine benchmark.

## Research anchors

- Tero et al., [Rules for biologically inspired adaptive network design](https://doi.org/10.1126/science.1177894), *Science* 2010 — Physarum balances transport cost, efficiency, and fault tolerance.
- Bonabeau, Dorigo, and Theraulaz, [Inspiration for optimization from social insect behaviour](https://doi.org/10.1038/35017500), *Nature* 2000 — stigmergy, evaporation, and decentralized optimization.
- Halim et al., [Stochastic Database Cracking](https://www.vldb.org/pvldb/vol5/p502_felixhalim_vldb2012.pdf), PVLDB 2012 — robust workload-driven physical reorganization.
- Kuhn et al., [Sleep recalibrates homeostatic and associative synaptic plasticity](https://doi.org/10.1038/ncomms12455), *Nature Communications* 2016 — downscaling restores adaptive capacity.
- Schapiro et al., [Human hippocampal replay prioritizes weakly learned information](https://doi.org/10.1038/s41467-018-06213-1), *Nature Communications* 2018 — the basis for weak-trace replay rather than frequency replay.
- Turrigiano-style homeostasis is experimentally supported by [conservation of total synaptic weight](https://doi.org/10.1038/nature01530), *Nature* 2003.
- Franco et al., [flow-sensing vascular remodelling](https://doi.org/10.7554/eLife.07727), *eLife* 2016, and Marbach et al., [time-delayed flow adaptation](https://pubmed.ncbi.nlm.nih.gov/36916885/), 2023 — local flow signals constrained by network-wide state.
- Giehl and von Wirén, [Root nutrient foraging](https://doi.org/10.1104/pp.114.245225), *Plant Physiology* 2014 — local architectural growth coupled to systemic resource status.
- Pohl and Dikic, [Cellular quality control by the ubiquitin-proteasome system and autophagy](https://doi.org/10.1126/science.aax3769), *Science* 2019 — selective recycling under hard homeostatic constraints.
- Wang et al., [Learning-based Progressive Cardinality Estimation](https://doi.org/10.1145/3588708), PACMMOD 2023, and Stillger et al., [LEO](https://www.vldb.org/conf/2001/P019.pdf), VLDB 2001 — actual-cardinality feedback and bounded reoptimization.
