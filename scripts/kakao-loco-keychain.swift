import Foundation
import Security
import LocalAuthentication

// Secrets travel only through this process's pipes, never command arguments.
struct Request: Decodable { let operation: String; let service: String; let account: String; let credential: Credential? }
struct Credential: Codable { let oauthToken: String; let userId: String; let deviceUuid: String; let deviceType: String }
enum HelperError: Error { case invalidRequest }
func valid(_ c: Credential) -> Bool {
    !c.oauthToken.isEmpty && !c.deviceUuid.isEmpty && c.deviceType == "tablet" &&
    c.userId.range(of: "^[1-9][0-9]*$", options: .regularExpression) != nil
}
func emit(_ value: [String: Any]) {
    if let data = try? JSONSerialization.data(withJSONObject: value, options: [.sortedKeys]) {
        FileHandle.standardOutput.write(data); FileHandle.standardOutput.write(Data([10]))
    }
}
func execute(_ r: Request) throws {
    guard ["get", "put", "exists"].contains(r.operation), !r.service.isEmpty, !r.account.isEmpty,
          r.service.count <= 256, r.account.count <= 256 else { throw HelperError.invalidRequest }
    var query: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: r.service, kSecAttrAccount as String: r.account,
        kSecAttrSynchronizable as String: false]
    if r.operation == "put" {
        guard let c = r.credential, valid(c) else { throw HelperError.invalidRequest }
        let data = try JSONEncoder().encode(c)
        // Deliberately refuse overwrite: a new account requires explicit migration.
        query[kSecValueData as String] = data
        query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let status = SecItemAdd(query as CFDictionary, nil)
        emit(["ok": status == errSecSuccess, "status": Int(status)])
        return
    }
    query[kSecMatchLimit as String] = kSecMatchLimitOne
    // Readiness queries never unlock Keychain or retrieve token data.
    let context = LAContext()
    context.interactionNotAllowed = true
    query[kSecUseAuthenticationContext as String] = context
    query[r.operation == "get" ? kSecReturnData as String : kSecReturnAttributes as String] = true
    var result: CFTypeRef?
    let status = SecItemCopyMatching(query as CFDictionary, &result)
    if r.operation == "exists" { emit(["ok": status == errSecSuccess, "exists": status == errSecSuccess, "status": Int(status)]); return }
    guard status == errSecSuccess, let data = result as? Data,
          let c = try? JSONDecoder().decode(Credential.self, from: data), valid(c) else {
        emit(["ok": false, "status": Int(status)]); return
    }
    let object = try JSONSerialization.jsonObject(with: JSONEncoder().encode(c))
    emit(["ok": true, "credential": object])
}

if CommandLine.arguments == [CommandLine.arguments[0], "--self-test"] {
    let synthetic = Credential(oauthToken: "fixture-only", userId: "42", deviceUuid: "test-device", deviceType: "tablet")
    guard valid(synthetic), !valid(Credential(oauthToken: "", userId: "0", deviceUuid: "", deviceType: "pc")) else { exit(1) }
    emit(["ok": true, "mode": "offline", "keychain_accessed": false]); exit(0)
}
guard CommandLine.arguments.count == 1 else { emit(["ok": false, "error": "no_arguments_allowed"]); exit(64) }
do {
    let input = FileHandle.standardInput.readDataToEndOfFile()
    guard input.count <= 32768 else { throw HelperError.invalidRequest }
    try execute(JSONDecoder().decode(Request.self, from: input))
} catch {
    // Do not interpolate errors or malformed input; they can contain secrets.
    emit(["ok": false, "error": "invalid_request"]); exit(65)
}
