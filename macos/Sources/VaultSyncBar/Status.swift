import CVaultSync
import Foundation

/// One vault as `vaultsync status --json --all` reports it.
struct VaultStatus: Decodable, Identifiable {
    struct Peer: Decodable, Identifiable {
        let id: String
        let name: String
        let online: Bool
        let last_seen: Int?
    }

    let folder: String
    let error: String?
    let machine: String?
    let pending: Int?
    let running: Bool?
    let started: Bool?
    let last_sync: Int?
    let peers: [Peer]?
    let conflicts: [String]?

    var id: String { folder }
    var name: String { URL(fileURLWithPath: folder).lastPathComponent }
}

/// The vaultsync engine, linked into this app: the same commands the
/// command-line tool runs, in-process. The engine does all the work; the app
/// only shows it.
enum CLI {
    @discardableResult
    static func run(_ args: [String]) throws -> String {
        let request = String(decoding: try JSONSerialization.data(withJSONObject: args), as: UTF8.self)
        guard let reply = request.withCString({ vs_command($0) }) else {
            throw CLIError(message: "no reply from the engine")
        }
        defer { vs_free(reply) }
        let object = try JSONSerialization.jsonObject(with: Data(String(cString: reply).utf8)) as? [String: String]
        if let output = object?["output"] { return output }
        throw CLIError(message: object?["error"] ?? "")
    }

    static func status() throws -> [VaultStatus] {
        let json = try run(["status", "--json", "--all"])
        return try JSONDecoder().decode([VaultStatus].self, from: Data(json.utf8))
    }
}

struct CLIError: LocalizedError {
    let message: String
    var errorDescription: String? { message.isEmpty ? "vaultsync failed" : message }
}
