import Foundation

// Protocol: one JSON request on stdin, exactly one JSON response on stdout.
// Diagnostics go to stderr. This process never runs commands, touches files on
// the model's behalf, or uses the network.

func diag(_ s: String) {
    FileHandle.standardError.write(Data((s + "\n").utf8))
}

func readStdin(limit: Int) -> Data {
    var data = Data()
    while data.count <= limit {
        let chunk = FileHandle.standardInput.readData(ofLength: 16 * 1024)
        if chunk.isEmpty { break }
        data.append(chunk)
    }
    return data
}

func emit(_ r: BridgeResponse) {
    do {
        var out = try r.json()
        out.append(0x0A)
        FileHandle.standardOutput.write(out)
    } catch {
        diag("cli2-apple-fm: could not encode response: \(error)")
        exit(70)
    }
}

if CommandLine.arguments.contains("--version") {
    print("cli2-apple-fm \(bridgeVersion)")
    exit(0)
}

let input = readStdin(limit: Limits.maxRequestBytes)
let request: BridgeRequest
do {
    request = try RequestDecoder.decode(input)
} catch let f as BridgeFailure {
    diag("cli2-apple-fm: \(f.code): \(f.message)")
    emit(BridgeResponse.failure("unknown", f))
    exit(2)
} catch {
    diag("cli2-apple-fm: \(error)")
    emit(BridgeResponse.failure("unknown", BridgeFailure(code: "malformed_request", message: "\(error)")))
    exit(2)
}

let response = await Resolver(backend: makeDefaultBackend()).handle(request)
if let e = response.error { diag("cli2-apple-fm: \(e.code): \(e.message)") }
emit(response)
exit(response.status == "error" ? 1 : 0)
