import Foundation
import Security

enum DeviceStore {
    private static let service = "bid.m1r4g3"
    private static let account = "bound-device"
    private static let group = "QG96QRRQBQ.bid.m1r4g3"

    static var exists: Bool { load() != nil }

    static func load() -> Data? {
        var query = baseQuery()
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var item: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &item) == errSecSuccess else {
            return nil
        }
        return item as? Data
    }

    static func save(_ json: String) throws {
        SecItemDelete(baseQuery() as CFDictionary)
        var query = baseQuery()
        query[kSecValueData as String] = Data(json.utf8)
        query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        let status = SecItemAdd(query as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw NSError(domain: NSOSStatusErrorDomain, code: Int(status))
        }
    }

    static func delete() {
        SecItemDelete(baseQuery() as CFDictionary)
    }

    private static func baseQuery() -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrAccessGroup as String: group,
        ]
    }
}
