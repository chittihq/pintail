# Architecture decisions

### Bun is the dashboard package manager

The goal specification originally named pnpm in the repository layout. The
owner subsequently directed Pintail to use Bun instead. Bun now owns dependency
installation, the lockfile, local scripts, CI, and container dashboard builds;
Yarn and pnpm artifacts are not accepted. This changes build tooling only and
does not change the Nuxt 4 plus shadcn-vue dashboard decision.
