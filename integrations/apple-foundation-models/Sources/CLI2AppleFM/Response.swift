import Foundation

struct BridgeRanking: Codable, Equatable {
    var capabilityId: String
    var rank: Int
    /// high | medium | low | none
    var relevance: String
    var reason: String
}

struct BridgeAvailability: Codable, Equatable {
    /// available | device_not_eligible | apple_intelligence_not_enabled | model_not_ready
    /// | unavailable_unknown | unsupported_platform
    var state: String
    var detail: String?
}

struct BridgeError: Codable, Equatable {
    var code: String
    var message: String
}

struct BridgeModelInfo: Codable, Equatable {
    var provider = "apple-foundation-models"
    var local = true
}

/// Exactly one of these is written to standard output.
struct BridgeResponse: Codable, Equatable {
    var schemaVersion = responseSchemaVersion
    var requestId: String
    /// ranked | no_match | clarification | expanded | available | unavailable | error
    var status: String
    var rankings: [BridgeRanking] = []
    var clarification: String?
    var terms: [String]?
    var availability: BridgeAvailability?
    var error: BridgeError?
    var bridgeVersion = CLI2AppleFM_bridgeVersion
    var model = BridgeModelInfo()
    var latencyMs: Int?

    enum CodingKeys: String, CodingKey {
        case schemaVersion, requestId, status, rankings, clarification, terms
        case availability, error, bridgeVersion, model, latencyMs
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(schemaVersion, forKey: .schemaVersion)
        try c.encode(requestId, forKey: .requestId)
        try c.encode(status, forKey: .status)
        try c.encode(rankings, forKey: .rankings)
        try c.encode(clarification, forKey: .clarification)  // explicit null when absent
        try c.encodeIfPresent(terms, forKey: .terms)
        try c.encodeIfPresent(availability, forKey: .availability)
        try c.encodeIfPresent(error, forKey: .error)
        try c.encode(bridgeVersion, forKey: .bridgeVersion)
        try c.encode(model, forKey: .model)
        try c.encodeIfPresent(latencyMs, forKey: .latencyMs)
    }

    static func failure(_ requestId: String, _ f: BridgeFailure, status: String = "error") -> BridgeResponse {
        BridgeResponse(
            requestId: requestId, status: status, error: BridgeError(code: f.code, message: f.message))
    }

    func json() throws -> Data {
        let e = JSONEncoder()
        e.keyEncodingStrategy = .convertToSnakeCase
        e.outputFormatting = [.sortedKeys]
        return try e.encode(self)
    }
}

let CLI2AppleFM_bridgeVersion = bridgeVersion
