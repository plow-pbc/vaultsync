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

/// Runs the vaultsync engine embedded in this app, which does all the work;
/// the app only shows it.
enum CLI {
    static let binary = Bundle.main.url(forAuxiliaryExecutable: "vaultsync")!
    static let environment: [String: String] = ProcessInfo.processInfo.environment

    @discardableResult
    static func run(_ args: [String]) throws -> String {
        let process = Process()
        process.executableURL = binary
        process.arguments = args
        process.environment = environment
        let out = Pipe(), err = Pipe()
        process.standardOutput = out
        process.standardError = err
        try process.run()
        // Drain stderr alongside stdout, so neither pipe can fill and stall git.
        var errData = Data()
        let drained = DispatchGroup()
        drained.enter()
        DispatchQueue.global().async {
            errData = err.fileHandleForReading.readDataToEndOfFile()
            drained.leave()
        }
        let data = out.fileHandleForReading.readDataToEndOfFile()
        drained.wait()
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
