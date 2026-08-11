<script setup lang="ts">
import { VisArea, VisLine, VisXYContainer } from '@unovis/vue'
import type { VitalsSample } from '@/composables/useVitals'

const props = defineProps<{
  label: string
  /// Reads the metric off a sample, so one card serves all three.
  value: (sample: VitalsSample) => number
  samples: VitalsSample[]
  /// Rendered beside the current reading.
  unit?: string
  /// Fixes the vertical scale. A percentage is always drawn 0-100 so a quiet
  /// process looks quiet; a rate autoscales, since its ceiling is unknown.
  max?: number
  decimals?: number
  color: string
  /// Shown under the value: the ceiling this metric is measured against.
  caption?: string
  /// How many samples the card is scaled for, whether or not it has them yet.
  window?: number
}>()

const windowSize = computed(() => props.window ?? 60)

/// Fixed to the whole window rather than the data extent.
///
/// Left to autoscale, three points stretch across the full width and the line
/// redraws its own shape on every sample - so a graph that has been collecting
/// for four seconds looks exactly like one that has been running for a minute.
/// Pinning the domain makes the line grow left to right and hold its shape,
/// and once the window is full it scrolls, because the oldest sample drops off
/// as the newest arrives.
const timeDomain = computed<[number, number]>(() => [0, windowSize.value - 1])

/// Positioned so the newest sample always sits on the right edge.
///
/// The x coordinate is the sample's distance from *now*, not its position in
/// the buffer, so a half-full window occupies the right-hand side and history
/// extends leftward into the empty space. "Now" therefore never moves: a
/// reading stays where it is drawn and travels left as it ages, rather than
/// the live end creeping across the card while the window fills.
const series = computed(() => {
  const offset = windowSize.value - props.samples.length
  return props.samples.map((sample, index) => ({
    index: offset + index,
    value: props.value(sample),
  }))
})

const current = computed(() => (series.value.at(-1)?.value ?? 0))

/// Autoscaled headroom, so a flat line does not sit on the ceiling.
const domain = computed<[number, number]>(() => {
  if (props.max !== undefined) return [0, props.max]
  const peak = Math.max(...series.value.map((point) => point.value), 0.001)
  return [0, peak * 1.25]
})

const formatted = computed(() => current.value.toFixed(props.decimals ?? 0))
</script>

<template>
  <Card class="overflow-hidden p-0">
    <div class="flex items-start justify-between gap-3 p-4 pb-2">
      <div>
        <p class="text-muted-foreground mb-1 font-mono text-xs font-bold tracking-[0.12em] uppercase">{{ label }}</p>
        <p class="text-2xl font-bold tabular-nums tracking-tight">
          {{ formatted }}<span v-if="unit" class="text-muted-foreground ml-1 text-base font-medium">{{ unit }}</span>
        </p>
      </div>
      <p v-if="caption" class="text-muted-foreground mt-1 text-right text-sm">{{ caption }}</p>
    </div>
    <!-- Fixed height so all three cards align regardless of their data. -->
    <div class="h-24 w-full">
      <VisXYContainer
        v-if="series.length > 1"
        :data="series"
        :y-domain="domain"
        :x-domain="timeDomain"
        :margin="{ top: 4, bottom: 2, left: 0, right: 0 }"
        :height="96"
      >
        <VisArea :x="(d: { index: number }) => d.index" :y="(d: { value: number }) => d.value" :color="color" :opacity="0.18" />
        <VisLine :x="(d: { index: number }) => d.index" :y="(d: { value: number }) => d.value" :color="color" :line-width="1.5" />
      </VisXYContainer>
      <!-- One point is not a line. Until a second sample arrives the card
           shows the reading it has rather than an empty frame. -->
      <div v-else class="text-muted-foreground grid h-full place-items-center text-sm">
        collecting…
      </div>
    </div>
  </Card>
</template>
