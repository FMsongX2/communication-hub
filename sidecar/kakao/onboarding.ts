import { closeSync, openSync, writeSync } from 'node:fs'
import { chmod, mkdir, readFile, writeFile } from 'node:fs/promises'
import { homedir } from 'node:os'
import { resolve, join } from 'node:path'
import { readCredentials, storeCredentials, validateCredentials, type KakaoCredentials } from './auth'

const service = 'dev.communication-hub.kakao-loco'
const account = 'owner-tablet'

// Neither console nor shell arguments ever receive account/password/passcode/token.
export async function hiddenInput(label: string, tty: number): Promise<string> {
  if (!process.stdin.isTTY || !process.stdout.isTTY) throw new Error('Owner interactive terminal required')
  writeSync(tty, label)
  const input = process.stdin
  const wasRaw = input.isRaw
  input.setRawMode(true)
  input.resume()
  return new Promise((resolveInput, reject) => {
    let value = ''
    const done = (error?: Error) => {
      input.off('data', receive)
      input.setRawMode(wasRaw)
      input.pause()
      writeSync(tty, '\n')
      if (error) reject(error); else resolveInput(value)
    }
    const receive = (chunk: Buffer) => {
      for (const char of chunk.toString('utf8')) {
        if (char === '\u0003' || char === '\u0004') { done(new Error('Owner cancelled')); return }
        if (char === '\r' || char === '\n') { done(); return }
        if (char === '\u007f' || char === '\b') value = value.slice(0, -1)
        else if (char >= ' ') value += char
      }
    }
    input.on('data', receive)
  })
}

export interface OwnerLoginDependencies {
  login: (options: { email: string; password: string; deviceType: 'tablet'; force: false; onPasscodeDisplay: (code: string) => void }) => Promise<any>
  prompt: (label: string) => Promise<string>
  showPasscode: (code: string) => void
  save: (credential: KakaoCredentials) => Promise<void>
}

export async function ownerLogin(deps: OwnerLoginDependencies): Promise<KakaoCredentials> {
  if (await deps.prompt('새 tablet session 연결에 동의하면 CONNECT 입력: ') !== 'CONNECT') throw new Error('Owner cancelled')
  let email = (await deps.prompt('Kakao account email (hidden): ')).trim()
  let password = await deps.prompt('Kakao password (hidden): ')
  if (!email || !password) throw new Error('Both owner inputs are required; desktop credential extraction is disabled')
  let result: any
  try {
    result = await deps.login({ email, password, deviceType: 'tablet', force: false, onPasscodeDisplay: deps.showPasscode })
  } catch { throw new Error('Kakao login failed; no credentials saved. Retry only from the owner terminal.') }
  finally { email = ''; password = '' }
  // Do not log upstream responses, messages, refresh tokens, or debug callbacks.
  if (!result?.authenticated || !result?.credentials?.access_token) {
    throw new Error('Kakao login was not completed; device slot or mobile approval may need owner attention. Force login is disabled.')
  }
  const c = result.credentials
  const credential = validateCredentials({ oauthToken: c.access_token, userId: c.user_id, deviceUuid: c.device_uuid, deviceType: c.device_type })
  await deps.save(credential)
  return credential
}

export async function writeCandidates(credential: KakaoCredentials, configPath: string, outputDir: string): Promise<void> {
  const original = JSON.parse(await readFile(configPath, 'utf8'))
  if (typeof original.socket !== 'string' || !original.socket) throw new Error('Existing Hub socket is required')
  await mkdir(outputDir, { recursive: true, mode: 0o700 })
  await chmod(outputDir, 0o700)
  const sidecarState = join(outputDir, 'state')
  await mkdir(sidecarState, { recursive: true, mode: 0o700 })
  await chmod(sidecarState, 0o700)
  const socket = join(outputDir, 'kakao.sock')
  if (Buffer.byteLength(socket) > 100) throw new Error('Sidecar Unix socket path exceeds safe macOS length')
  const config = { socket_path: socket, hub_socket: original.socket, state_dir: sidecarState,
    expected_user_id: credential.userId, credential_service: service, credential_account: account,
    allowed_chat_ids: [], mode: 'shadow', attachment_roots: [] }
  const hub = { ...original, dispatch_enabled: false, external_auto_send: false,
    kakao: { ...original.kakao, loco: { socket, expected_user_id: credential.userId, mode: 'shadow', rooms: {} } } }
  // Exclusive creation prevents silently replacing reviewed room bindings.
  await writeFile(join(outputDir, 'sidecar.shadow.json'), JSON.stringify(config, null, 2) + '\n', { mode: 0o600, flag: 'wx' })
  await writeFile(join(outputDir, 'hub.shadow.candidate.json'), JSON.stringify(hub, null, 2) + '\n', { mode: 0o600, flag: 'wx' })
}

async function main(): Promise<void> {
  const command = process.argv[2]
  if (!['login', 'stage'].includes(command) || process.argv.length > 5) throw new Error('usage: onboarding.ts login|stage [existing-config] [new-output-directory]')
  if (!process.stdin.isTTY || !process.stdout.isTTY) throw new Error('Run from the owner interactive terminal; redirected input/output is refused')
  const configPath = resolve(process.argv[3] ?? join(homedir(), 'Library/Application Support/CommunicationHub/config.json'))
  const outputDir = resolve(process.argv[4] ?? join(homedir(), '.communication-hub/kakao-loco'))
  if (Buffer.byteLength(join(outputDir, 'kakao.sock')) > 100) throw new Error('Sidecar Unix socket path exceeds safe macOS length')
  // Validate the source and destination before attempting a network login.
  const original = JSON.parse(await readFile(configPath, 'utf8'))
  if (typeof original.socket !== 'string' || !original.socket) throw new Error('Existing Hub socket is required')
  for (const file of ['sidecar.shadow.json', 'hub.shadow.candidate.json']) {
    if (await Bun.file(join(outputDir, file)).exists()) throw new Error('Candidate already exists; choose a new output directory')
  }
  const tty = openSync('/dev/tty', 'w')
  try {
    writeSync(tty, 'Owner-only Kakao tablet onboarding. Live Hub settings and running services remain untouched.\nMobile approval may be required; an occupied slot will not be forced.\n')
    const credential = command === 'stage' ? await readCredentials(service, account) : await ownerLogin({
      login: async options => {
        const { loginFlow } = await import('@communication-hub/agent-messenger-kakao/auth')
        return loginFlow(options)
      },
      prompt: label => hiddenInput(label, tty),
      showPasscode: code => writeSync(tty, `\n휴대폰 KakaoTalk 승인 화면에 입력할 code: ${code}\n`),
      save: c => storeCredentials(service, account, c),
    })
    await writeCandidates(credential, configPath, outputDir)
    writeSync(tty, `Keychain credential ready. Shadow candidates written to ${outputDir}\nNo listener, launch agent, or live Hub config was activated. Review room IDs before starting the pilot.\n`)
  } finally { closeSync(tty) }
}

if (import.meta.main) main().catch(error => {
  // Only messages generated in this module are safe to print; filesystem paths and upstream detail are omitted.
  const message = error instanceof Error ? error.message : ''
  const safe = /^(Owner |Kakao login |Both owner |Run from |Existing Hub |Candidate already |Keychain |Invalid Keychain |Sidecar Unix |usage:)/.test(message)
  process.stderr.write((safe ? message : 'Onboarding failed. No live configuration was changed; inspect local prerequisites.') + '\n')
  process.exitCode = 1
})
