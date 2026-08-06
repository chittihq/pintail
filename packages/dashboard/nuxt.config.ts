import tailwindcss from '@tailwindcss/vite'

// `nuxt dev` serves the app on its own origin, but the dashboard calls the
// API with relative paths (`/api/...`, `/status`) because in production
// pintail serves the built assets itself and same-origin is enough. In dev
// those paths would hit Nuxt, so forward them to a running pintail.
//
// Point PINTAIL_API_URL at that server. It defaults to a local one; when the
// server runs on a remote docker host, open a tunnel first so the port is
// local — see the dashboard section of docs/development.md.
const pintailApi = process.env.PINTAIL_API_URL ?? 'http://127.0.0.1:8080'

export default defineNuxtConfig({
  $development: {
    nitro: {
      devProxy: {
        // `/api/events` is server-sent events, so the proxy must not buffer
        // or the dashboard's live updates arrive only when the stream ends.
        '/api': { target: `${pintailApi}/api`, changeOrigin: true, ws: true },
        '/status': { target: `${pintailApi}/status`, changeOrigin: true },
      },
    },
  },
  compatibilityDate: '2026-07-30',
  css: ['~/assets/css/main.css'],
  devtools: { enabled: false },
  modules: ['shadcn-nuxt'],
  shadcn: {
    prefix: '',
    componentDir: '@/components/ui',
  },
  vite: {
    plugins: [tailwindcss()],
  },
  app: {
    head: {
      htmlAttrs: { lang: 'en' },
      meta: [
        { charset: 'utf-8' },
        { name: 'viewport', content: 'width=device-width, initial-scale=1' },
        {
          name: 'description',
          content: 'Pintail turns live MySQL data into fast columnar analytics.',
        },
      ],
      link: [{ rel: 'icon', type: 'image/svg+xml', href: '/favicon.svg' }],
      title: 'Pintail',
    },
  },
})
