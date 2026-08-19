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
const CONTESTED_EMAIL = 'contested@pintail.local'
const REVOKED_EMAIL = 'revoked@pintail.local'
const FIRST_WORKSPACE = 'My workspace'
const SECOND_WORKSPACE = 'Second workspace'
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
/// Everything the server has written so far, readable mid-run.
const serverOutput: string[] = []

async function drainInto(stream: ReadableStream<Uint8Array>) {
  const reader = stream.getReader()
  const decoder = new TextDecoder()
  for (;;) {
    const chunk = await reader.read()
    if (chunk.done) return
    serverOutput.push(decoder.decode(chunk.value, { stream: true }))
  }
}

function serverLog(): string {
  return serverOutput.join('')
}

/// Waits for a line the server has not necessarily written yet: the response
/// reaches the browser before the log is flushed here.
async function waitForServerLog(pattern: RegExp, timeoutMs = 10_000) {
  for (let waited = 0; waited < timeoutMs; waited += 200) {
    if (pattern.test(serverLog())) return
    await Bun.sleep(200)
  }
  throw new Error(`the server never logged ${pattern}`)
}
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
  // Drained incrementally rather than awaited as one promise at exit: the
  // server's own log is evidence a check needs *during* the run. A refusal
  // that reaches the browser as a generic message is only diagnosable from
  // the line the server wrote, so a check has to be able to read it.
  void drainInto(pintailProcess.stdout as ReadableStream<Uint8Array>)
  void drainInto(pintailProcess.stderr as ReadableStream<Uint8Array>)
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

  await check('an admin changes a teammate\'s role and it survives a reload', async () => {
    const member = page!.getByRole('row').filter({ hasText: GOOGLE_INVITE_EMAIL }).first()
    await member.getByRole('combobox').click()
    await page!.getByRole('option', { name: 'Admin' }).click()
    // Reload rather than trusting the control: the row updates optimistically,
    // so reading it back straight away would pass even if the request failed.
    await page!.reload()
    await page!.getByRole('heading', { name: 'Team' }).waitFor()
    const reloaded = page!.getByRole('row').filter({ hasText: GOOGLE_INVITE_EMAIL }).first()
    await reloaded.getByRole('combobox').waitFor({ timeout: 20_000 })
    if (!(await reloaded.getByText('Admin').first().isVisible())) {
      throw new Error('the role change did not persist')
    }
  })

  await check('nobody can change their own role', async () => {
    // The last admin demoting themselves would leave the workspace with no
    // one able to administer it, so the row offers no control at all - the
    // server refuses it too, which is what actually enforces this.
    const own = page!.getByRole('row').filter({ hasText: OPERATOR.email }).first()
    await own.waitFor({ timeout: 20_000 })
    if (await own.getByRole('combobox').count()) {
      throw new Error('an admin was offered a control to change their own role')
    }
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

  await check('a second-workspace invite admits an account that already exists', async () => {
    // The invitee already has a user row and a Google subject from the check
    // above. Before the invite reached that branch, sign-in matched the
    // subject and returned immediately, so this invite would have stayed
    // pending forever while they landed back in their first workspace.
    //
    // This is the same code path that repairs an account left with no
    // membership at all - the state that made a valid invite unusable and
    // could not be fixed by re-sending it.
    await page!.goto(pintailUrl)
    await page!.getByRole('button', { name: 'Pintail' }).click()
    await page!.getByRole('menuitem', { name: 'Create workspace' }).click()
    const dialog = page!.getByRole('dialog')
    await dialog.getByRole('heading', { name: 'Create a workspace' }).waitFor()
    await dialog.getByLabel('Name').fill(SECOND_WORKSPACE)
    await dialog.getByRole('button', { name: 'Create workspace' }).click()
    await dialog.waitFor({ state: 'hidden', timeout: 15_000 })
    // Creating it also switches to it, so the invite below is issued into the
    // second workspace rather than the first.
    await page!.getByText(SECOND_WORKSPACE).first().waitFor({ timeout: 20_000 })

    await page!.goto(`${pintailUrl}/team`)
    await page!.getByRole('heading', { name: 'Team' }).waitFor()
    await page!.getByLabel('Email').fill(GOOGLE_INVITE_EMAIL)
    await page!.getByRole('combobox').first().click()
    await page!.getByRole('option', { name: 'Operator' }).click()
    await page!.getByRole('button', { name: 'Invite', exact: true }).click()
    const link = page!.getByTestId('invite-link')
    await link.waitFor({ timeout: 20_000 })
    const secondInvite = (await link.textContent())?.trim() || ''
    const target = new URL(secondInvite)

    const { context, visitor } = await signInWithGoogle(target.pathname + target.search, {
      email: GOOGLE_INVITE_EMAIL,
      sub: 'google-subject-invitee',
      emailVerified: true,
    })
    try {
      await visitor.getByText('Node healthy').waitFor({ timeout: 30_000 })
      // The workspace they land in is the assertion. Merely signing in proves
      // nothing here: the subject already exists, so the old behaviour also
      // "succeeded" - it just dropped them back into their first workspace and
      // left this invite pending forever.
      await visitor.getByText(SECOND_WORKSPACE).first().waitFor({ timeout: 30_000 })
    } finally {
      await context.close()
    }
  })

  await check('removing a member revokes their live session immediately', async () => {
    // The invitee holds a valid, unexpired token for the second workspace.
    // Authorization used to be read from that token, so removal changed
    // nothing until it expired - up to twelve hours during which a removed
    // admin could keep working, and issue fresh admin invites that renew the
    // access indefinitely.
    const { context, visitor } = await signInWithGoogle('/', {
      email: GOOGLE_INVITE_EMAIL,
      sub: 'google-subject-invitee',
      emailVerified: true,
    })
    try {
      await visitor.getByText('Node healthy').waitFor({ timeout: 30_000 })

      // Remove them from the workspace their session is actually in. Their
      // default is the first workspace, while the admin is still switched to
      // the second from the check above, so removing without switching back
      // would revoke a membership the live session never uses.
      await page!.goto(pintailUrl)
      await page!.getByRole('button', { name: 'Pintail' }).click()
      await page!.getByRole('menuitem', { name: FIRST_WORKSPACE }).click()
      await page!
        .getByRole('button', { name: 'Pintail' })
        .filter({ hasText: FIRST_WORKSPACE })
        .waitFor({ timeout: 15_000 })

      await page!.goto(`${pintailUrl}/team`)
      await page!.getByRole('heading', { name: 'Team' }).waitFor()
      const member = page!.getByRole('row').filter({ hasText: GOOGLE_INVITE_EMAIL })
      await member.first().waitFor({ timeout: 20_000 })
      await member.first().getByRole('button', { name: 'Remove member' }).click()
      // The trash icon now opens a confirmation dialog rather than being the
      // deletion itself; the removal this check measures happens on confirm.
      await page!.getByRole('dialog').getByRole('button', { name: 'Remove member' }).click()

      // Their next call must be refused, without waiting for expiry.
      await waitForServerLog(/GET \/session 401|GET \/databases 401|GET \/activity 401/, 30_000)
    } finally {
      await context.close()
    }
  })

  await check('the opened invite decides the workspace, not the newest one', async () => {
    // The confused-deputy case. Two invites are open for one address, in
    // different workspaces and with different roles; the visitor follows the
    // OLDER one. Selecting the newest claimable invite across the node - which
    // is what an address search does - let an admin of any workspace aim a
    // newer, higher-privileged invite at someone and capture them when they
    // followed a legitimate invite elsewhere.
    async function inviteTo(workspace: string, role: string) {
      await page!.goto(pintailUrl)
      await page!.getByRole('button', { name: 'Pintail' }).click()
      await page!.getByRole('menuitem', { name: workspace }).click()
      await page!
        .getByRole('button', { name: 'Pintail' })
        .filter({ hasText: workspace })
        .waitFor({ timeout: 15_000 })
      await page!.goto(`${pintailUrl}/team`)
      await page!.getByRole('heading', { name: 'Team' }).waitFor()
      await page!.getByLabel('Email').fill(CONTESTED_EMAIL)
      await page!.getByRole('combobox').first().click()
      await page!.getByRole('option', { name: role }).click()
      await page!.getByRole('button', { name: 'Invite', exact: true }).click()
      const link = page!.getByTestId('invite-link')
      await link.waitFor({ timeout: 20_000 })
      return (await link.textContent())?.trim() || ''
    }

    const wanted = await inviteTo(FIRST_WORKSPACE, 'Viewer')
    await inviteTo(SECOND_WORKSPACE, 'Admin')

    const target = new URL(wanted)
    const { context, visitor } = await signInWithGoogle(target.pathname + target.search, {
      email: CONTESTED_EMAIL,
      sub: 'google-subject-contested',
      emailVerified: true,
    })
    try {
      await visitor.getByText('Node healthy').waitFor({ timeout: 30_000 })
      await visitor.getByText(FIRST_WORKSPACE).first().waitFor({ timeout: 30_000 })
      if ((await visitor.getByText(SECOND_WORKSPACE).count()) > 0) {
        throw new Error('the newer invite captured a visitor who followed an older one')
      }
    } finally {
      await context.close()
    }
  })

  await check('a revoked invite is refused as spent, not as never sent', async () => {
    await page!.goto(`${pintailUrl}/team`)
    await page!.getByRole('heading', { name: 'Team' }).waitFor()
    await page!.getByLabel('Email').fill(REVOKED_EMAIL)
    await page!.getByRole('combobox').first().click()
    await page!.getByRole('option', { name: 'Viewer' }).click()
    await page!.getByRole('button', { name: 'Invite', exact: true }).click()
    const row = page!.getByRole('row').filter({ hasText: REVOKED_EMAIL })
    await row.first().waitFor({ timeout: 20_000 })
    await row.first().getByRole('button', { name: 'Revoke invite' }).click()
    await page!.getByText('Invite revoked').waitFor({ timeout: 20_000 })

    const { context, visitor } = await signInWithGoogle('/', {
      email: REVOKED_EMAIL,
      sub: 'google-subject-revoked',
      emailVerified: true,
    })
    try {
      // "Not invited" would be wrong and unhelpful here: the invite existed,
      // an admin revoked it, and the recipient needs a new one rather than to
      // go re-checking which Google account they picked.
      await visitor.getByText(/no longer usable/).waitFor({ timeout: 30_000 })
      await waitForServerLog(new RegExp(`oauth refused ${REVOKED_EMAIL}: .*revoked`))
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
      await visitor.getByText(/has not been invited/).waitFor({ timeout: 30_000 })
      // The browser is told only that it was refused. Which address was
      // refused - the thing that actually resolves the report - exists solely
      // in the server log, so assert it is there and names the account.
      await waitForServerLog(
        /oauth refused an address at pintail.local: no invite exists/,
      )
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
      await waitForServerLog(new RegExp(`oauth refused ${OPERATOR.email}: an account already exists`))
    } finally {
      await context.close()
    }
  })

  await check('a forged callback cannot cancel a sign-in in progress', async () => {
    // Driven over HTTP rather than through a page: the property is about
    // exactly which Set-Cookie headers two responses carry, and a browser
    // hides that behind its cookie jar.
    const started = await fetch(`${pintailUrl}/api/auth/google/start`, { redirect: 'manual' })
    const issued = started.headers.get('set-cookie') ?? ''
    if (!/pintail_oauth_state=[^;]+/.test(issued)) {
      throw new Error(`starting a sign-in did not issue a state cookie: ${issued}`)
    }
    const cookie = issued.split(';')[0]!

    // The forgery: a callback carrying only ?error=, which is what a
    // cross-site top-level navigation can produce. It must be refused for
    // failing to prove it belongs to this sign-in...
    const forged = await fetch(`${pintailUrl}/api/auth/google/callback?error=access_denied`, {
      redirect: 'manual',
      headers: { cookie },
    })
    const location = forged.headers.get('location') ?? ''
    if (!location.includes('auth_error=unverified_state')) {
      throw new Error(`a forged callback was not rejected as unverified: ${location}`)
    }
    // ...and crucially must not expire the cookie the real sign-in is still
    // relying on, which is how this cancelled a sign-in already in progress.
    const cleared = forged.headers.get('set-cookie') ?? ''
    if (/pintail_oauth_state=;|Max-Age=0/.test(cleared)) {
      throw new Error(`a forged callback cleared the live sign-in state: ${cleared}`)
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
    const captured = redactBootSecrets(serverLog()).trim()
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
