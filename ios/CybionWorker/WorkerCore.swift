import Foundation
import UIKit

/// Thin Swift wrapper around the Rust worker core (rust/src/entry.rs).
struct WorkerCore {
    /// Everything the worker persists lives here: worker.toml, the pending
    /// pairing and worker.log. Documents/ is also the working directory of
    /// `bash` calls (the in-process engine) and is shared with the Files app.
    let configDirectory: URL

    init(fileManager: FileManager = .default) {
        configDirectory = fileManager.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("CybionWorker", isDirectory: true)
    }

    var appVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0"
    }

    /// Starts the engine; false means the native side rejected the start.
    func start() -> Bool {
        let info: [String: String] = [
            "hostname": UIDevice.current.name,
            "platform": "ios \(UIDevice.current.systemVersion) (arm64)",
            "version": appVersion,
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: info),
              let json = String(data: data, encoding: .utf8) else {
            return false
        }
        return cybion_worker_start(configDirectory.path, json)
    }

    func stop() {
        cybion_worker_stop()
    }

    func reset() -> Bool {
        cybion_worker_reset(configDirectory.path)
    }

    /// The serialized status snapshot, decoded; nil when unavailable.
    func status() -> [String: Any]? {
        guard let pointer = cybion_worker_status() else { return nil }
        defer { cybion_worker_string_free(pointer) }
        let text = String(cString: pointer)
        guard let data = text.data(using: .utf8) else { return nil }
        return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    }
}
