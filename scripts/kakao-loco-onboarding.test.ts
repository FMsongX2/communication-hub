import { describe, expect, test } from 'bun:test'
import { mkdtemp, readFile, rm, stat, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { tmpdir } from 'node:os'
import { readCredentials, storeCredentials, validateCredentials } from '../sidecar/kakao/auth'
import { ownerLogin, writeCandidates } from '../sidecar/kakao/onboarding'

const fixture = { oauthToken: 'TEST-NOT-A-TOKEN', userId: '42', deviceUuid: 'test-device', deviceType: 'tablet' as const }
describe('owner-only onboarding, offline', () => {
  test('explicit inputs, tablet slot and no-force flow; no debug callback', async () => {
    const answers = ['CONNECT', 'fixture@example.invalid', 'TEST-NOT-A-PASSWORD']; let saved: unknown
    const c = await ownerLogin({ prompt: async () => answers.shift()!, showPasscode: () => {}, save: async c => { saved = c }, login: async opts => {
      expect(opts.deviceType).toBe('tablet'); expect(opts.force).toBe(false)
      expect(Object.keys(opts).sort()).toEqual(['deviceType', 'email', 'force', 'onPasscodeDisplay', 'password'])
      return { authenticated: true, credentials: { access_token: fixture.oauthToken, user_id: fixture.userId, device_uuid: fixture.deviceUuid, device_type: 'tablet' } }
    } })
    expect(c).toEqual(fixture); expect(saved).toEqual(fixture)
  })
  test('missing owner input never invokes login or desktop fallback', async () => {
    const answers = ['CONNECT', '', 'password']; let calls = 0
    await expect(ownerLogin({ prompt: async () => answers.shift()!, showPasscode: () => {}, save: async () => {}, login: async () => { calls++; return {} } })).rejects.toThrow('Both owner inputs')
    expect(calls).toBe(0)
  })
  test('occupied device slot never forces a second attempt', async () => {
    const answers = ['CONNECT', 'fixture@example.invalid', 'password']; let calls = 0; let saves = 0
    await expect(ownerLogin({ prompt: async () => answers.shift()!, showPasscode: () => {}, save: async () => { saves++ }, login: async () => { calls++; return { authenticated: false, next_action: 'choose_device' } } })).rejects.toThrow('Force login is disabled')
    expect(calls).toBe(1); expect(saves).toBe(0)
  })
  test('Keychain helper request uses only stdin contract', async () => {
    let request: unknown
    await storeCredentials('test-service', 'test-account', fixture, async r => { request = r; return { ok: true } })
    expect(request).toEqual({ operation: 'put', service: 'test-service', account: 'test-account', credential: fixture })
    expect(await readCredentials('test-service', 'test-account', async () => ({ ok: true, credential: fixture }))).toEqual(fixture)
    await expect(storeCredentials('test-service', 'test-account', fixture, async () => ({ ok: false, status: -25299 }))).rejects.toThrow('never overwritten')
    expect(() => validateCredentials({ ...fixture, deviceType: 'pc' })).toThrow()
    expect(() => validateCredentials({ ...fixture, userId: '0' })).toThrow()
  })
  test('shadow candidates preserve source; contain no token or approved room guesses', async () => {
    const dir = await mkdtemp('/tmp/loco-onboarding-')
    try {
      const config = join(dir, 'live.json'); const target = join(dir, 'candidate')
      const source = JSON.stringify({ socket: '/tmp/existing-hub.sock', dispatch_enabled: true, kakao: { enabled: true } })
      await writeFile(config, source)
      await writeCandidates(fixture, config, target)
      expect(await readFile(config, 'utf8')).toBe(source)
      const sidecarText = await readFile(join(target, 'sidecar.shadow.json'), 'utf8')
      expect(sidecarText).not.toContain(fixture.oauthToken)
      expect(JSON.parse(sidecarText)).toMatchObject({ mode: 'shadow', allowed_chat_ids: [], attachment_roots: [], expected_user_id: '42' })
      expect(JSON.parse(await readFile(join(target, 'hub.shadow.candidate.json'), 'utf8'))).toMatchObject({ dispatch_enabled: false, external_auto_send: false, kakao: { loco: { rooms: {}, mode: 'shadow' } } })
      expect((await stat(join(target, 'sidecar.shadow.json'))).mode & 0o777).toBe(0o600)
      await expect(writeCandidates(fixture, config, target)).rejects.toThrow()
    } finally { await rm(dir, { recursive: true, force: true }) }
  })
  test('device UUID is a bounded filename component, including official onboarding UUIDs', () => {
    for (const deviceUuid of ['abcdef0123456789'.repeat(4), '123e4567-e89b-12d3-a456-426614174000', 'agent-messenger_42', 'a'.repeat(128)]) {
      expect(validateCredentials({ ...fixture, deviceUuid }).deviceUuid).toBe(deviceUuid)
    }
    for (const deviceUuid of ['', '../escape', 'x/../../escape', 'x\\escape', '.', '..', 'with space', 'x\n', 'x\0', 'a'.repeat(129)]) {
      expect(() => validateCredentials({ ...fixture, deviceUuid })).toThrow('Invalid Keychain credential shape')
    }
  })
})
