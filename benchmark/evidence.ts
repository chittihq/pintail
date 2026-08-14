export type ComparableTiming = {
  name: string
  timings: Record<string, { minMs: number }>
}

export type ComparableReport = {
  methodology?: { hostFingerprint?: string; engineFingerprint?: string }
  queries?: ComparableTiming[]
}

export function pintailSpecificMinimumRegressions(
  previous: ComparableReport | undefined,
  current: ComparableReport,
): string[] {
  if (
    !previous?.methodology?.hostFingerprint ||
    previous.methodology.hostFingerprint !== current.methodology?.hostFingerprint ||
    !previous.methodology.engineFingerprint ||
    previous.methodology.engineFingerprint !== current.methodology?.engineFingerprint
  ) {
    return []
  }
  const before = new Map(previous.queries?.map((query) => [query.name, query]))
  return (current.queries ?? []).flatMap((query) => {
    const old = before.get(query.name)
    const oldPintail = old?.timings.pintail?.minMs
    const newPintail = query.timings.pintail?.minMs
    const oldControl = old?.timings.clickhouseFinal?.minMs
    const newControl = query.timings.clickhouseFinal?.minMs
    if (!oldPintail || !newPintail || !oldControl || !newControl) return []
    const pintailRatio = newPintail / oldPintail
    const controlRatio = newControl / oldControl
    return pintailRatio >= 1.5 && controlRatio >= 0.8 && controlRatio <= 1.2
      ? [
          `${query.name}: Pintail minimum ${pintailRatio.toFixed(2)}x, control ${controlRatio.toFixed(2)}x`,
        ]
      : []
  })
}
