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
    let last_sync: Int?
    let peers: [Peer]?
    let conflicts: [String]?

    var id: String { folder }
    var name: String { URL(fileURLWithPath: folder).lastPathComponent }
}

/// Runs the vaultsync CLI, which does all the work; this app only shows it.
enum CLI {
    static let binary = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent(".local/bin/vaultsync").path

    @discardableResult
    static func run(_ args: [String]) throws -> String {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: binary)
        process.arguments = args
        var env = ProcessInfo.processInfo.environment
        env["PATH"] = "/usr/bin:/bin:/usr/local/bin:/opt/homebrew/bin"
        process.environment = env
        let out = Pipe(), err = Pipe()
        process.standardOutput = out
        process.standardError = err
        try process.run()
        let data = out.fileHandleForReading.readDataToEndOfFile()
        let errData = err.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            throw CLIError(message: String(decoding: errData, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines))
        }
        return String(decoding: data, as: UTF8.self)
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
