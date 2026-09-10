export default defineNuxtPlugin({
  name: 'browser-telemetry',
  async setup(nuxtApp) {
    try {
      // The same static bundle serves every deployment. Read only public
      // reporting configuration, with a deadline so telemetry cannot stall boot.
      const response = await fetch('/api/telemetry/config', { signal: AbortSignal.timeout(2000) })
      if (!response.ok) return
      const config = await response.json() as { dsn: string | null; environment: string; release: string }
      if (!config.dsn) return
      const { init, captureException } = await import('@sentry/vue')
      const client = init({
        app: nuxtApp.vueApp,
        dsn: config.dsn,
        environment: config.environment,
        release: config.release,
        attachProps: false,
        // Nuxt owns Vue's error handler; subscribe to its hooks below.
        attachErrorHandler: false,
        sendDefaultPii: false,
        maxBreadcrumbs: 0,
        integrations: defaults => defaults.filter(integration => !['BrowserSession', 'Breadcrumbs'].includes(integration.name)),
        enableLogs: false,
        enableMetrics: false,
        initialScope: { tags: { surface: 'dashboard' } },
        beforeSend(event) {
          // Navigation URLs can hold sign-in codes and table names. Component
          // state and request details do not belong in a browser error report.
          delete event.request
          delete event.breadcrumbs
          delete event.user
          delete event.extra
          delete event.transaction
          delete event.contexts?.vue
          for (const exception of event.exception?.values ?? []) {
            for (const frame of exception.stacktrace?.frames ?? []) {
              if (frame.filename) frame.filename = frame.filename.split(/[?#]/)[0]
              if (frame.abs_path) frame.abs_path = frame.abs_path.split(/[?#]/)[0]
            }
          }
          return event
        },
      })
      const report = (error: unknown) => { captureException(error) }
      const removeVueHook = nuxtApp.hook('vue:error', report)
      const removeAppHook = nuxtApp.hook('app:error', report)
      if (import.meta.hot) {
        import.meta.hot.dispose(() => {
          removeVueHook()
          removeAppHook()
          void client?.close(0)
        })
      }
    } catch {
      // Reporting is optional. Offline or misconfigured telemetry must not
      // prevent sign-in or turn an otherwise working dashboard into an error.
    }
  },
})
