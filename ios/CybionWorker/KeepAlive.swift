import AVFoundation
import Foundation

/// Keeps the worker process running while the switch is on. iOS suspends
/// ordinary apps within seconds of backgrounding, so the app holds a
/// `playback` audio session playing a silent buffer - the standard mechanism
/// for user-enabled long running personal agents. The battery cost is real,
/// which is why the session only runs while **Enable Worker** is on.
final class KeepAlive {
    private var player: AVAudioPlayer?
    private var interruptionObserver: NSObjectProtocol?

    var isActive: Bool { player?.isPlaying == true }

    func start() {
        let session = AVAudioSession.sharedInstance()
        try? session.setCategory(.playback, mode: .default)
        try? session.setActive(true)
        let player = self.player ?? Self.makePlayer()
        guard let player else { return }
        player.play()
        self.player = player
        if interruptionObserver == nil {
            interruptionObserver = NotificationCenter.default.addObserver(
                forName: AVAudioSession.interruptionNotification,
                object: session,
                queue: .main
            ) { [weak self] notification in
                let raw = notification.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt
                if raw == AVAudioSession.InterruptionType.ended.rawValue {
                    self?.player?.play()
                }
            }
        }
    }

    func stop() {
        player?.stop()
        player = nil
        try? AVAudioSession.sharedInstance()
            .setActive(false, options: .notifyOthersOnDeactivation)
    }

    private static func makePlayer() -> AVAudioPlayer? {
        try? AVAudioPlayer(data: silentWave(seconds: 1))
    }

    /// One second of 8 kHz mono PCM silence wrapped in a WAV container.
    private static func silentWave(seconds: Int) -> Data {
        let sampleRate: UInt32 = 8_000
        let channels: UInt16 = 1
        let bits: UInt16 = 16
        let dataSize = Int(sampleRate) * seconds * Int(channels) * Int(bits) / 8
        var data = Data()

        func append(_ text: String) { data.append(contentsOf: Array(text.utf8)) }
        func append(_ value: UInt32, _ bytes: Int) {
            for index in 0..<bytes { data.append(UInt8((value >> (8 * index)) & 0xff)) }
        }
        func append(_ value: UInt16, _ bytes: Int) {
            for index in 0..<bytes { data.append(UInt8((value >> (8 * index)) & 0xff)) }
        }

        append("RIFF"); append(UInt32(36 + dataSize), 4); append("WAVE")
        append("fmt "); append(UInt32(16), 4)
        append(UInt16(1), 2) // PCM
        append(channels, 2)
        append(sampleRate, 4)
        append(sampleRate * UInt32(channels) * UInt32(bits) / 8, 4)
        append(channels * bits / 8, 2)
        append(bits, 2)
        append("data"); append(UInt32(dataSize), 4)
        data.append(Data(count: dataSize))
        return data
    }
}
