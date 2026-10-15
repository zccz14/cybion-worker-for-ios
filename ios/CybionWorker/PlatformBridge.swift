import UIKit

/// The C callbacks the Rust core invokes for actions only the app layer can
/// perform. Values are copied by `cybion_worker_register_platform`, so the
/// bridge only needs to live for the duration of the call.
enum PlatformBridge {
    static func register() {
        var bridge = CybionPlatform(context: nil, open_url: cybionOpenURLCallback)
        cybion_worker_register_platform(&bridge)
    }
}

/// C function pointers cannot be formed from closures that capture context,
/// so the callback lives at file scope and routes into `open(url:)`.
private func cybionOpenURLCallback(
    _ context: UnsafeMutableRawPointer?,
    _ url: UnsafePointer<CChar>?
) -> Int32 {
    guard let url else { return 0 }
    return PlatformBridge.open(url: String(cString: url))
}

extension PlatformBridge {
    /// Blocking wrapper used from the core's worker threads; UIKit needs the
    /// main thread, so hop over and wait for the completion handler.
    fileprivate static func open(url: String) -> Int32 {
        guard let url = URL(string: url) else { return 0 }
        let result = ResultBox()
        let done = DispatchSemaphore(value: 0)
        DispatchQueue.main.async {
            UIApplication.shared.open(url, options: [:]) { success in
                result.value = success ? 1 : 0
                done.signal()
            }
        }
        if done.wait(timeout: .now() + 15) == .timedOut { return 0 }
        return result.value
    }

    fileprivate final class ResultBox: @unchecked Sendable {
        var value: Int32 = 0
    }
}
