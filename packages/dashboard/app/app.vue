<script setup lang="ts">
import { Badge } from '@/components/ui/badge'

useHead({
  bodyAttrs: {
    class: 'dashboard-body',
  },
})
</script>

<template>
  <main class="shell">
    <header class="masthead">
      <a class="wordmark" href="/" aria-label="Pintail home">
        <span class="wordmark-mark" aria-hidden="true">PT</span>
        <span>Pintail</span>
      </a>
      <Badge as="div" variant="ghost" class="boot-state" role="status">
        <span class="boot-state-dot" aria-hidden="true" />
        Engine ready
      </Badge>
    </header>

    <section class="hero" aria-labelledby="hero-title">
      <div class="hero-copy">
        <p class="eyebrow">Single-node analytical database</p>
        <h1 id="hero-title">Columnar analytics<br>for MySQL.</h1>
        <p class="lede">
          The control plane is running. Connect a source to start building a
          live, queryable mirror.
        </p>
        <Badge as="p" variant="outline" class="milestone">
          M0 · System initialized
        </Badge>
      </div>

      <div class="column-flight" aria-hidden="true">
        <div class="column column-one">
          <span />
          <span />
          <span />
          <span />
          <span />
        </div>
        <div class="column column-two">
          <span />
          <span />
          <span />
          <span />
        </div>
        <div class="column column-three">
          <span />
          <span />
          <span />
          <span />
          <span />
          <span />
        </div>
      </div>
    </section>

    <footer>
      <span>Local control plane</span>
      <span class="footer-rule" />
      <span>Zero external services</span>
    </footer>
  </main>
</template>
