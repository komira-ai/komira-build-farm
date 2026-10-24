// Stores or reads a generic-password item in the data-protection keychain of the user it
// runs as: `keychain-probe add <service>` prints `ADDED <variant> <OSStatus>` for each
// variant; `keychain-probe read <service>` prints `FOUND <variant> <status>` or
// `NOT_FOUND <variant> <status>`. Compiled with swiftc by tests/slot_clean/keychain.rs.
import Foundation
import Security

let arguments = CommandLine.arguments
guard arguments.count == 3 else {
    print("usage: keychain-probe add|read <service>")
    exit(2)
}
let mode = arguments[1]
let service = arguments[2]

// The data-protection keychain, with the accessibility after first unlock and with the
// default one, and the user's default file keychain.
typealias Variant = (name: String, dp: Bool, accessible: CFString?)
let variants: [Variant] = [
    ("after-first-unlock", true, kSecAttrAccessibleAfterFirstUnlock),
    ("default", true, nil),
    ("file-default", false, nil),
]

func query(_ variant: Variant) -> [String: Any] {
    return [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: service,
        kSecAttrAccount as String: "kbf-\(variant.name)",
        kSecUseDataProtectionKeychain as String: variant.dp,
    ]
}

switch mode {
case "add":
    for variant in variants {
        var item = query(variant)
        item[kSecValueData as String] = Data("kbf-keychain-secret".utf8)
        if let accessible = variant.accessible {
            item[kSecAttrAccessible as String] = accessible
        }
        let status = SecItemAdd(item as CFDictionary, nil)
        print("ADDED \(variant.name) \(status)")
    }
case "read":
    for variant in variants {
        var item = query(variant)
        item[kSecMatchLimit as String] = kSecMatchLimitOne
        item[kSecReturnData as String] = true
        var found: CFTypeRef?
        let status = SecItemCopyMatching(item as CFDictionary, &found)
        print("\(status == errSecSuccess ? "FOUND" : "NOT_FOUND") \(variant.name) \(status)")
    }
default:
    print("unknown mode \(mode)")
    exit(2)
}
