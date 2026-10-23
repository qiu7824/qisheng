import Foundation

func measure(_ name: String) {
    var samples: [Double] = []
    for _ in 0..<12 {
        let start = DispatchTime.now().uptimeNanoseconds
        Thread.sleep(forTimeInterval: 0.02)
        samples.append(Double(DispatchTime.now().uptimeNanoseconds - start) / 1_000_000)
    }
    let result: [String: Any] = ["activity": name, "requested_ms": 20, "observed_ms": samples]
    let json = try! JSONSerialization.data(withJSONObject: result, options: [.sortedKeys])
    print(String(data: json, encoding: .utf8)!)
}

measure("none")
let info = ProcessInfo.processInfo
let token = info.beginActivity(options: [.latencyCritical, .userInitiatedAllowingIdleSystemSleep], reason: "Native audio deadline validation")
measure("latency-critical")
info.endActivity(token)
measure("ended")
