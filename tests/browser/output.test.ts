import { describe, expect, test } from 'bun:test'
import { redactBootSecrets } from './output'

describe('redactBootSecrets', () => {
  test('removes both first-boot credentials without hiding diagnostics', () => {
    const output = [
      'pintail first boot — save this secret now:',
      'PINTAIL_DSN_ENCRYPTION_KEY=dsn-secret',
      'secrets saved to /tmp/pintail/secrets.toml',
      'PINTAIL_JWT_SECRET=jwt-secret',
      'pintail listening on http://127.0.0.1:8080',
    ].join('\n')

    expect(redactBootSecrets(output)).toBe([
      'pintail first boot — save this secret now:',
      'PINTAIL_DSN_ENCRYPTION_KEY=<redacted>',
      'secrets saved to /tmp/pintail/secrets.toml',
      'PINTAIL_JWT_SECRET=<redacted>',
      'pintail listening on http://127.0.0.1:8080',
    ].join('\n'))
  })
})
