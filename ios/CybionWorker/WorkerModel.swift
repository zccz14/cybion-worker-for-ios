import Combine
import Foundation
import UIKit

/// Single screen state: the switch, the live status snapshot, pairing and the
/// exit hatches (retry / reset) - the same shape as the Android Worker app.
@MainActor
final class WorkerModel: ObservableObject {
    @Published private(set) var phaseName = "stopped"
    @Published private(set) var statusLines: [String] = []
    @Published private(set) var pairingCode: String?
    @Published private(set) var pairingURL: String?
    @Published private(set) var canRetry = false
    @Published private(set) var canReset = false
    @Published private(set) var keepAliveActive = false
    @Published var enabled: Bool

    private let core: WorkerCore
    private let keepAlive = KeepAlive()
    private var timer: Timer?

    private static let enabledKey = "workerEnabled"

    init(core: WorkerCore = WorkerCore()) {
        self.core = core
        enabled = UserDefaults.standard.bool(forKey: Self.enabledKey)
    }

    var version: String { core.appVersion }

    func begin() {
        if enabled {
            startWorker()
        }
        render()
        timer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.tick() }
        }
    }

    func end() {
        timer?.invalidate()
        timer = nil
    }

    func setEnabled(_ value: Bool) {
        enabled = value
        UserDefaults.standard.set(value, forKey: Self.enabledKey)
        if value {
            startWorker()
        } else {
            core.stop()
            keepAlive.stop()
        }
        render()
    }

    func retry() {
        startWorker()
        render()
    }

    func reset() {
        guard !(core.status()?["running"] as? Bool ?? false) else { return }
        _ = core.reset()
        render()
    }

    func openPairingPage() {
        guard let pairingURL, let url = URL(string: pairingURL) else { return }
        UIApplication.shared.open(url)
    }

    /// Runs on every timer tick and whenever the app comes back to the screen.
    func tick() {
        if enabled && !keepAlive.isActive {
            keepAlive.start()
        }
        render()
    }

    private func startWorker() {
        keepAlive.start()
        _ = core.start()
    }

    private func render() {
        keepAliveActive = keepAlive.isActive
        let snapshot = core.status()
        let phase = snapshot?["phase"] as? [String: Any]
        let name = phase?["name"] as? String ?? "stopped"
        phaseName = name

        var lines: [String] = []
        switch name {
        case "starting":
            lines.append(String(localized: "status_starting"))
        case "pairing":
            lines.append(String(localized: "status_pairing"))
        case "connecting":
            lines.append(String(localized: "status_connecting"))
        case "online":
            lines.append(String(localized: "status_online"))
        case "reconnecting":
            let attempt = phase?["attempt"] as? Int ?? 0
            lines.append(String(format: String(localized: "status_reconnecting"), attempt))
        case "failed":
            let message = phase?["message"] as? String ?? ""
            lines.append(String(format: String(localized: "status_failed"), message))
        default:
            lines.append(String(localized: "status_stopped"))
        }
        if let machineID = snapshot?["machine_id"] as? String, !machineID.isEmpty {
            lines.append(String(format: String(localized: "status_worker_id"), machineID))
        }
        if let hostname = snapshot?["hostname"] as? String, !hostname.isEmpty {
            lines.append(String(format: String(localized: "status_hostname"), hostname))
        }
        statusLines = lines

        if name == "pairing" {
            pairingCode = phase?["user_code"] as? String
            pairingURL = phase?["url"] as? String
        } else {
            pairingCode = nil
            pairingURL = nil
        }
        let running = snapshot?["running"] as? Bool ?? false
        canRetry = name == "failed"
        canReset = name == "stopped" && !running
    }
}
