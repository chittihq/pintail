/// Sign-in gate: the invite and "Continue with Google" paths, driven end to
/// end in a real browser against a stand-in for Google.
///
/// Separate from the smoke suite deliberately. Authentication needs neither a
/// MySQL source nor an object store, so this gate boots only the release
/// pintail binary and Chromium - no Docker at all - and finishes in seconds.
/// The smoke suite covers replication, which needs both.
///
/// Google itself cannot be driven headlessly: bot detection, a real consent
/// screen, real credentials. So this serves the same three shapes Google
/// does - authorize, token, userinfo - and Pintail is pointed at it through
/// the PINTAIL_GOOGLE_*_URL overrides. Without that seam the invite path has
/// no coverage at all, which is how it shipped broken: it is the *only* way a
/// teammate ever gets an account, so every defect on it reaches a real user
/// first.
///
/// Run with: bun run auth
///           PINTAIL_E2E_BINARY=... bun run auth
///
/// Screenshots land in tests/browser/artifacts/ on every failure.

import { createServer } from 'node:net'
import { mkdirSync, mkdtempSync, rmSync } from 'node:fs'
import { homedir, tmpdir } from 'node:os'
import { join, resolve } from 'node:path'
import { chromium } from 'playwright'
import type { Browser, Page } from 'playwright'
import { redactBootSecrets } from './output'

const repository = resolve(import.meta.dir, '..', '..')
const artifacts = join(import.meta.dir, 'artifacts')
const cargoBinary = join(homedir(), '.cargo', 'bin', 'cargo')
const cargoTargetDir = join(repository, 'target')

const OPERATOR = { email: 'auth@pintail.local', password: 'browser-auth-password' }
const GOOGLE_INVITE_EMAIL = 'googler@pintail.local'
const GOOGLE_STRANGER_EMAIL = 'stranger@pintail.local'
const GOOGLE_CLIENT = { id: 'browser-gate-client', secret: 'browser-gate-client-secret' }

interface CheckResult {
  check: string
  status: 'PASS' | 'FAIL'
  detail?: string
}

interface GoogleIdentity {
  email: string
  sub: string
  emailVerified: boolean
}

const results: CheckResult[] = []
const pageErrors: string[] = []
let pintailProcess: ReturnType<typeof Bun.spawn> | undefined
let pintailStdout: Promise<string> | undefined
let pintailStderr: Promise<string> | undefined
let browser: Browser | undefined
let page: Page | undefined
let pintailDataDir = ''
let pintailUrl = ''

function log(message: string) {
  console.log(`[auth] ${message}`)
}

async function command(args: string[], options: { quiet?: boolean } = {}) {
  const child = Bun.spawn(args, {
    cwd: repository,
    env: { ...process.env, CARGO_TARGET_DIR: cargoTargetDir },
    stdout: 'pipe',
    stderr: 'pipe',
  })
  const [stdout, stderr, status] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ])
  if (status !== 0) {
    throw new Error(`${args.join(' ')} failed with ${status}\n${stdout.trim()}\n${stderr.trim()}`)
  }
  if (!options.quiet && stderr.trim()) console.error(stderr.trim())
  return { stdout: stdout.trim() }
}

async function freePort(): Promise<number> {
  return new Promise((resolvePort, reject) => {
    const server = createServer()
    server.once('error', reject)
    server.listen(0, '127.0.0.1', () => {
      const address = server.address()
      if (!address || typeof address === 'string') {
        server.close()
        reject(new Error('could not allocate a local port'))
        return
      }
      server.close((error) => (error ? reject(error) : resolvePort(address.port)))
    })
  })
}

async function buildPintail(): Promise<string> {
  if (process.env.PINTAIL_E2E_BINARY) return resolve(process.env.PINTAIL_E2E_BINARY)
  log('building the release pintail binary')
  await command([cargoBinary, 'build', '--release', '-p', 'pintail'])
  const metadata = await command([cargoBinary, 'metadata', '--format-version', '1', '--no-deps'], {
    quiet: true,
  })
  return join(JSON.parse(metadata.stdout).target_directory, 'release', 'pintail')
}

async function check(name: string, action: () => Promise<void>) {
  try {
    await action()
    results.push({ check: name, status: 'PASS' })
    log(`PASS ${name}`)
  } catch (error) {
    const detail = error instanceof Error ? error.message : String(error)
    results.push({ check: name, status: 'FAIL', detail })
    log(`FAIL ${name} — ${detail}`)
    for (const captured of pageErrors.slice(-5)) log(`  browser ${captured}`)
    if (page) {
      mkdirSync(artifacts, { recursive: true })
      const file = join(artifacts, `auth-${name.replaceAll(/[^a-z0-9]+/gi, '-')}.png`)
      await page.screenshot({ path: file, fullPage: true }).catch(() => {})
      log(`screenshot: ${file}`)
    }
  }
}

/// The stand-in for Google. Authorization codes are single-use here exactly
/// as they are at Google, so a replayed callback fails the way a real one
/// would rather than succeeding twice.
let googleIdentity: GoogleIdentity = {
  email: GOOGLE_INVITE_EMAIL,
  sub: 'google-subject-invitee',
  emailVerified: true,
}
let googleServer: ReturnType<typeof Bun.serve> | undefined
let googleOrigin = ''
const googleCodes = new Map<string, GoogleIdentity>()
const googleAccessTokens = new Map<string, GoogleIdentity>()
/// Every authorize request Pintail sent, so a check can assert what it asked
/// Google for and not only what came back.
const googleAuthorizeRequests: URL[] = []

function startGoogleStub() {
  googleServer = Bun.serve({
    hostname: '127.0.0.1',
    port: 0,
    async fetch(request) {
      const url = new URL(request.url)
      if (url.pathname === '/o/oauth2/v2/auth') {
        googleAuthorizeRequests.push(url)
        const redirectUri = url.searchParams.get('redirect_uri')
        const state = url.searchParams.get('state')
        if (!redirectUri || !state) {
          return new Response('missing redirect_uri or state', { status: 400 })
        }
        const code = `stub-code-${googleCodes.size + 1}`
        googleCodes.set(code, googleIdentity)
        const back = new URL(redirectUri)
        back.searchParams.set('code', code)
        back.searchParams.set('state', state)
        return new Response(null, { status: 302, headers: { location: back.toString() } })
      }
      if (url.pathname === '/token' && request.method === 'POST') {
        const form = new URLSearchParams(await request.text())
        const code = form.get('code') ?? ''
        const identity = googleCodes.get(code)
        googleCodes.delete(code)
        if (!identity) return new Response('invalid_grant', { status: 400 })
        if (form.get('client_secret') !== GOOGLE_CLIENT.secret) {
          return new Response('invalid_client', { status: 401 })
        }
        const accessToken = `stub-access-${googleAccessTokens.size + 1}`
        googleAccessTokens.set(accessToken, identity)
        return Response.json({ access_token: accessToken, token_type: 'Bearer' })
      }
      if (url.pathname === '/v1/userinfo') {
        const bearer = (request.headers.get('authorization') ?? '').replace(/^Bearer /, '')
        const identity = googleAccessTokens.get(bearer)
        if (!identity) return new Response('invalid_token', { status: 401 })
        return Response.json({
          sub: identity.sub,
          email: identity.email,
          email_verified: identity.emailVerified,
        })
      }
      return new Response('not found', { status: 404 })
    },
  })
  googleOrigin = `http://127.0.0.1:${googleServer.port}`
  log(`google stand-in on ${googleOrigin}`)
}

/// Walks one full "Continue with Google" round trip in an isolated context,
/// so no admin session left in localStorage can make a refused sign-in look
/// like a successful one.
async function signInWithGoogle(startPath: string, identity: GoogleIdentity) {
  googleIdentity = identity
  const context = await browser!.newContext({ viewport: { width: 1440, height: 900 } })
  const visitor = await context.newPage()
  visitor.setDefaultTimeout(20_000)
  visitor.on('pageerror', (error) => pageErrors.push(`pageerror: ${error.message}`))
  visitor.on('console', (message) => {
    if (message.type() === 'error') pageErrors.push(`console: ${message.text()}`)
  })
  await visitor.goto(`${pintailUrl}${startPath}`)
  await visitor.getByRole('button', { name: /Continue with Google/ }).click()
  // The click leaves for the stand-in, which bounces to the callback, which
  // bounces to "/". Only the document is awaited here: a successful sign-in
  // holds the /events stream open forever, so "networkidle" never arrives and
  // every passing case would time out. The caller asserts what it expects to
  // find, which is the honest signal anyway.
  await visitor.waitForLoadState('domcontentloaded')
  return { context, visitor }
}

async function main() {
  startGoogleStub()
  const binary = await buildPintail()
  pintailDataDir = mkdtempSync(join(tmpdir(), 'pintail-auth-'))
  const httpPort = await freePort()
  const wirePort = await freePort()
  pintailUrl = `http://127.0.0.1:${httpPort}`
  pintailProcess = Bun.spawn(
    [
      binary,
      '--data-dir',
      pintailDataDir,
      '--http-bind',
      `127.0.0.1:${httpPort}`,
      '--wire-bind',
      `127.0.0.1:${wirePort}`,
    ],
    {
      cwd: repository,
      stdout: 'pipe',
      stderr: 'pipe',
      env: {
        ...process.env,
        // The server half of the exchange. The browser half follows the
        // authorize URL the server builds, so both halves stay on the
        // stand-in without the test having to intercept anything.
        PINTAIL_GOOGLE_AUTH_URL: `${googleOrigin}/o/oauth2/v2/auth`,
        PINTAIL_GOOGLE_TOKEN_URL: `${googleOrigin}/token`,
        PINTAIL_GOOGLE_USERINFO_URL: `${googleOrigin}/v1/userinfo`,
      },
    },
  )
  pintailStdout = new Response(pintailProcess.stdout).text()
  pintailStderr = new Response(pintailProcess.stderr).text()
  for (let attempt = 0; ; attempt += 1) {
    try {
      const response = await fetch(`${pintailUrl}/health`)
      if (response.ok) break
    } catch {}
    if (attempt >= 240) throw new Error('pintail did not become healthy within 120 seconds')
    await Bun.sleep(500)
  }

  browser = await chromium.launch()
  page = await browser.newPage({ viewport: { width: 1440, height: 900 } })
  page.setDefaultTimeout(20_000)
  page.on('pageerror', (error) => pageErrors.push(`pageerror: ${error.message}`))
  page.on('console', (message) => {
    if (message.type() === 'error') pageErrors.push(`console: ${message.text()}`)
  })

  await check('the operator account is created on first boot', async () => {
    await page!.goto(pintailUrl)
    await page!.getByRole('heading', { name: 'Create the operator' }).waitFor()
    await page!.getByLabel('Email').fill(OPERATOR.email)
    await page!.getByLabel('Password').fill(OPERATOR.password)
    await page!.getByRole('button', { name: 'Initialize Pintail' }).click()
    await page!.getByText('Node healthy').waitFor()
  })

  await check('Google sign-in is configured and enabled', async () => {
    await page!.goto(`${pintailUrl}/settings`)
    await page!.getByRole('heading', { name: 'Google sign-in' }).waitFor()
    await page!.getByLabel('Public URL').fill(pintailUrl)
    await page!.getByLabel('Client ID').fill(GOOGLE_CLIENT.id)
    await page!.getByLabel('Client secret').fill(GOOGLE_CLIENT.secret)
    await page!.getByRole('switch').last().click()
    await page!.getByRole('button', { name: 'Save Google settings' }).click()

    // The public status endpoint is what the login page reads to decide
    // whether to offer the button, so it is the honest assertion.
    const status = await page!.request.get(`${pintailUrl}/api/auth/google/status`)
    if (!(await status.json()).enabled) {
      throw new Error('Google sign-in did not become enabled after saving')
    }
  })

  await check('the login page offers Google once it is enabled', async () => {
    const context = await browser!.newContext()
    try {
      const visitor = await context.newPage()
      await visitor.goto(pintailUrl)
      await visitor.getByRole('button', { name: /Continue with Google/ }).waitFor()
    } finally {
      await context.close()
    }
  })

  let inviteLink = ''
  await check('an invite is issued to a Google address', async () => {
    await page!.goto(`${pintailUrl}/team`)
    await page!.getByRole('heading', { name: 'Team' }).waitFor()
    await page!.getByLabel('Email').fill(GOOGLE_INVITE_EMAIL)
    await page!.getByRole('combobox').first().click()
    await page!.getByRole('option', { name: 'Viewer' }).click()
    await page!.getByRole('button', { name: 'Invite', exact: true }).click()
    const link = page!.getByTestId('invite-link')
    await link.waitFor({ timeout: 20_000 })
    inviteLink = (await link.textContent())?.trim() || ''
    if (!inviteLink.includes('/accept-invite?token=')) {
      throw new Error(`invite link looks wrong: ${inviteLink}`)
    }
  })

  await check('the invite page names the workspace to a signed-out visitor', async () => {
    const context = await browser!.newContext()
    try {
      const visitor = await context.newPage()
      await visitor.goto(inviteLink)
      // The address and role are shown so the recipient can tell a real
      // invite from a phishing link.
      await visitor.getByText(GOOGLE_INVITE_EMAIL).waitFor({ timeout: 20_000 })
      await visitor.getByText('viewer').first().waitFor()
      await visitor.getByRole('button', { name: /Continue with Google/ }).waitFor()
    } finally {
      await context.close()
    }
  })

  await check('an invited teammate joins by signing in with Google', async () => {
    const target = new URL(inviteLink)
    const { context, visitor } = await signInWithGoogle(target.pathname + target.search, {
      email: GOOGLE_INVITE_EMAIL,
      sub: 'google-subject-invitee',
      emailVerified: true,
    })
    try {
      // Landing signed in is the whole point: the account created, the
      // membership granted and the invite consumed in one pass.
      await visitor.getByText('Node healthy').waitFor({ timeout: 30_000 })
      if ((await visitor.getByRole('button', { name: 'Sign in' }).count()) > 0) {
        throw new Error('the invitee was returned to the login form')
      }
      const authorize = googleAuthorizeRequests.at(-1)
      if (authorize?.searchParams.get('client_id') !== GOOGLE_CLIENT.id) {
        throw new Error(`authorize used client_id ${authorize?.searchParams.get('client_id')}`)
      }
      const expected = `${pintailUrl}/api/auth/google/callback`
      if (authorize?.searchParams.get('redirect_uri') !== expected) {
        throw new Error(`authorize used redirect_uri ${authorize?.searchParams.get('redirect_uri')}`)
      }
    } finally {
      await context.close()
    }
  })

  await check('the joined teammate is a member and the invite is spent', async () => {
    await page!.goto(`${pintailUrl}/team`)
    await page!.getByRole('heading', { name: 'Team' }).waitFor()
    const member = page!.getByRole('row').filter({ hasText: GOOGLE_INVITE_EMAIL })
    await member.first().waitFor({ timeout: 20_000 })
    // The invite must read accepted rather than pending: a membership granted
    // while the invite stayed open would let the same link admit a second
    // account.
    await page!.getByText('accepted', { exact: true }).first().waitFor({ timeout: 20_000 })
  })

  await check('a returning Google teammate signs straight back in', async () => {
    // No invite is left to redeem, so this covers the branch that matches an
    // existing Google subject instead of the invite branch.
    const { context, visitor } = await signInWithGoogle('/', {
      email: GOOGLE_INVITE_EMAIL,
      sub: 'google-subject-invitee',
      emailVerified: true,
    })
    try {
      await visitor.getByText('Node healthy').waitFor({ timeout: 30_000 })
    } finally {
      await context.close()
    }
  })

  await check('an uninvited Google account is refused with a reason', async () => {
    const { context, visitor } = await signInWithGoogle('/', {
      email: GOOGLE_STRANGER_EMAIL,
      sub: 'google-subject-stranger',
      emailVerified: true,
    })
    try {
      await visitor.getByText('has not been invited').waitFor({ timeout: 30_000 })
    } finally {
      await context.close()
    }
  })

  await check('a password account is told to link rather than silently refused', async () => {
    // The operator signed up with a password, so Google must not quietly
    // create a second account for the same address.
    const { context, visitor } = await signInWithGoogle('/', {
      email: OPERATOR.email,
      sub: 'google-subject-operator',
      emailVerified: true,
    })
    try {
      await visitor.getByText('then link Google from Settings').waitFor({ timeout: 30_000 })
    } finally {
      await context.close()
    }
  })

  await check('an unverified Google email cannot sign in', async () => {
    const { context, visitor } = await signInWithGoogle('/', {
      email: 'unverified@pintail.local',
      sub: 'google-subject-unverified',
      emailVerified: false,
    })
    try {
      await visitor.getByText(/invalid or expired|Google sign-in failed/).waitFor({ timeout: 30_000 })
    } finally {
      await context.close()
    }
  })

  const failed = results.filter((result) => result.status === 'FAIL')
  log(
    `gate: ${failed.length === 0 ? 'PASS' : 'FAIL'} (${results.length - failed.length} passed, ${failed.length} failed)`,
  )
  if (failed.length > 0) process.exitCode = 1
}

async function cleanup() {
  googleServer?.stop(true)
  await browser?.close().catch(() => {})
  pintailProcess?.kill()
  await pintailProcess?.exited.catch(() => {})
  if (process.exitCode) {
    const [stdout, stderr] = await Promise.all([
      pintailStdout ?? Promise.resolve(''),
      pintailStderr ?? Promise.resolve(''),
    ])
    const captured = redactBootSecrets(`${stdout}${stderr}`).trim()
    if (captured) console.error(captured)
  }
  if (pintailDataDir) rmSync(pintailDataDir, { recursive: true, force: true })
}

try {
  await main()
} catch (error) {
  log(`fatal: ${error instanceof Error ? error.message : String(error)}`)
  process.exitCode = 1
} finally {
  await cleanup()
}
