import Foundation

let requestSchemaVersion = "cli2.apple_fm.request.v1"
let responseSchemaVersion = "cli2.apple_fm.response.v1"
let bridgeVersion = "0.1.0"

/// Bounds enforced before anything reaches the model.
enum Limits {
    static let maxRequestBytes = 64 * 1024
    static let maxCandidates = 40
    static let maxIntentChars = 1000
    static let maxFieldChars = 240
    static let maxHelpChars = 400
}

struct BridgeConstraints: Codable, Equatable {
    var modifyFiles: Bool?
    var network: String?
}

struct BridgeCandidate: Codable, Equatable {
    var id: String
    var name: String
    var purpose: String?
    var interfaceCompleteness: String?
    var effects: String?
    var kind: String?
    var helpExcerpt: String?
}

struct BridgeRequest: Codable, Equatable {
    var schemaVersion: String
    var requestId: String
    /// "rank" (default), "expand" or "status".
    var operation: String?
    var intent: String?
    var constraints: BridgeConstraints?
    var candidates: [BridgeCandidate]?

    var op: String { operation ?? "rank" }
}

/// A request that cannot be served, with a stable code.
struct BridgeFailure: Error, Equatable {
    var code: String
    var message: String
}

enum RequestDecoder {
    static func decode(_ data: Data) throws -> BridgeRequest {
        if data.count > Limits.maxRequestBytes {
            throw BridgeFailure(
                code: "request_too_large",
                message: "request is \(data.count) bytes; the limit is \(Limits.maxRequestBytes)")
        }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let request: BridgeRequest
        do {
            request = try decoder.decode(BridgeRequest.self, from: data)
        } catch {
            throw BridgeFailure(code: "malformed_request", message: "request is not valid: \(error)")
        }
        try validate(request)
        return request
    }

    static func validate(_ r: BridgeRequest) throws {
        guard r.schemaVersion == requestSchemaVersion else {
            throw BridgeFailure(
                code: "unsupported_schema",
                message: "unsupported schema_version \(r.schemaVersion); expected \(requestSchemaVersion)")
        }
        guard !r.requestId.isEmpty, r.requestId.count <= 128 else {
            throw BridgeFailure(code: "malformed_request", message: "request_id must be 1-128 characters")
        }
        switch r.op {
        case "status":
            return
        case "rank", "expand":
            guard let intent = r.intent, !intent.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                throw BridgeFailure(code: "malformed_request", message: "intent is required")
            }
            guard intent.count <= Limits.maxIntentChars else {
                throw BridgeFailure(
                    code: "request_too_large",
                    message: "intent exceeds \(Limits.maxIntentChars) characters")
            }
            if r.op == "rank" {
                guard let cands = r.candidates, !cands.isEmpty else {
                    throw BridgeFailure(code: "malformed_request", message: "candidates must not be empty")
                }
                guard cands.count <= Limits.maxCandidates else {
                    throw BridgeFailure(
                        code: "request_too_large",
                        message: "\(cands.count) candidates exceed the limit of \(Limits.maxCandidates)")
                }
                var seen = Set<String>()
                for c in cands {
                    guard !c.id.isEmpty, !c.name.isEmpty else {
                        throw BridgeFailure(code: "malformed_request", message: "candidate id and name are required")
                    }
                    guard seen.insert(c.id).inserted else {
                        throw BridgeFailure(code: "malformed_request", message: "duplicate candidate id \(c.id)")
                    }
                }
            }
        default:
            throw BridgeFailure(code: "malformed_request", message: "unknown operation \(r.op)")
        }
    }
}
