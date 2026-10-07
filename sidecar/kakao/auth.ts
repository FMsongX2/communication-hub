import { resolve } from 'node:path'

export interface KakaoCredentials {
  oauthToken: string
  userId: string
  deviceUuid: string
  deviceType: 'tablet'
}

export function validateCredentials(value: unknown): KakaoCredentials {
  const v = value as Partial<KakaoCredentials> | null
  if (!v || typeof v.oauthToken !== 'string' || !v.oauthToken ||
      // The SDK embeds this identifier in its sync-state filename.
      typeof v.deviceUuid !== 'string' || !/^[A-Za-z0-9_-]{1,128}$/.test(v.deviceUuid) ||
      typeof v.userId !== 'string' || !/^[1-9][0-9]*$/.test(v.userId) || v.deviceType !== 'tablet') {
    throw new Error('Invalid Keychain credential shape')
  }
  return { oauthToken: v.oauthToken, userId: v.userId, deviceUuid: v.deviceUuid, deviceType: 'tablet' }
}

export type KeychainRunner = (request: Record<string, unknown>) => Promise<Record<string, unknown>>
export const runKeychain: KeychainRunner = async request => {
  const helper = process.env.KAKAO_LOCO_KEYCHAIN_HELPER ?? resolve(import.meta.dir, '../../local/kakao-loco-migration/bin/keychain-helper')
  const child = Bun.spawn([helper], { stdin: 'pipe', stdout: 'pipe', stderr: 'ignore' })
  child.stdin.write(JSON.stringify(request))
  child.stdin.end()
  const text = await new Response(child.stdout).text()
  if (await child.exited !== 0) throw new Error('Keychain helper failed; credential not returned')
  try { return JSON.parse(text) } catch { throw new Error('Invalid Keychain helper response') }
}

export async function readCredentials(service: string, account: string, run: KeychainRunner = runKeychain): Promise<KakaoCredentials> {
  const response = await run({ operation: 'get', service, account })
  if (response.ok !== true) throw new Error(`Keychain credential unavailable (status ${Number(response.status)})`)
  return validateCredentials(response.credential)
}

export async function storeCredentials(service: string, account: string, credential: KakaoCredentials, run: KeychainRunner = runKeychain): Promise<void> {
  const response = await run({ operation: 'put', service, account, credential: validateCredentials(credential) })
  if (response.ok !== true) throw new Error(`Keychain insert refused (status ${Number(response.status)}); existing credentials are never overwritten`)
}
