import Foundation

#if canImport(FoundationModels)
import FoundationModels
#endif

// MARK: - Model output, as picked by the model (indices, never free-form IDs)

/// What the model decides about a candidate. Candidates are referred to by the
/// number shown in the prompt so the model cannot produce an unknown ID.
struct ModelPick: Equatable {
    var number: Int
    var relevance: String
    var reason: String
}

struct ModelRankingOutput: Equatable {
    var outcome: String  // ranked | no_match | clarification
    var picks: [ModelPick]
    var clarification: String?
}

/// The only thing the resolver needs from a language model. The real
/// implementation is Apple's; tests supply a scripted one.
protocol ModelBackend {
    func availability() -> BridgeAvailability
    func rank(instructions: String, prompt: String) async throws -> ModelRankingOutput
    func expand(instructions: String, prompt: String) async throws -> [String]
}

// MARK: - Prompt construction

enum Prompts {
    static let rankInstructions = """
        You choose which installed command-line tool or project task a user would use to accomplish their intent.
        You are given the intent and a numbered list of candidates. Judge each candidate by what it does:
        use its description when there is one, and otherwise your own knowledge of what a program with that
        name does (for example `curl` fetches URLs, `jq` processes JSON, `tar` makes archives).
        Rules:
        - Refer to candidates only by their number. Never invent tools, commands, or arguments.
        - A candidate is relevant only if doing its job would accomplish the intent. A candidate that
          merely shares a word with the intent, or does something only loosely related, is not relevant
          (building is not deploying; running tests is not searching). Prefer returning no picks to
          picking a poor match.
        - Order relevant candidates from best to worst. Mark each high, medium, low or none.
        - Respect the stated constraints. Missing information is unknown, never safe.
        - If no candidate could accomplish the intent, return no picks and the outcome no_match.
        - Use the clarification outcome only when two or more candidates would do clearly different
          things that the intent does not distinguish, and then ask one short question.
        - Never claim anything was or will be executed or approved.
        """

    static let expandInstructions = """
        You help a command-line tool search a list of installed program names.
        Given a user's intent, list names of well-known command-line programs, and short words that could
        appear in a project task name, that would accomplish it. Single lowercase words only.
        Examples:
        - "turn this video into a gif" -> ffmpeg, convert, gif, video
        - "how many lines of code are there" -> wc, cloc, tokei, count
        """

    static func clip(_ s: String?, _ n: Int) -> String? {
        guard let s = s?.trimmingCharacters(in: .whitespacesAndNewlines), !s.isEmpty else { return nil }
        let oneLine = s.replacingOccurrences(of: "\n", with: " ")
        return oneLine.count > n ? String(oneLine.prefix(n)) + "…" : oneLine
    }

    static func rankPrompt(_ r: BridgeRequest) -> String {
        var lines = ["Intent: \(clip(r.intent, Limits.maxIntentChars) ?? "")"]
        var constraints: [String] = []
        if let m = r.constraints?.modifyFiles {
            constraints.append(m ? "files may be modified" : "files must not be modified")
        }
        if let n = r.constraints?.network, n != "unknown" {
            constraints.append("network: \(n)")
        }
        lines.append("Constraints: " + (constraints.isEmpty ? "none stated" : constraints.joined(separator: "; ")))
        lines.append("Candidates:")
        for (i, c) in (r.candidates ?? []).enumerated() {
            var parts = ["[\(i + 1)] \(clip(c.name, 80) ?? c.name)"]
            if let k = clip(c.kind, 40) { parts.append("kind: \(k)") }
            parts.append("purpose: " + (clip(c.purpose, Limits.maxFieldChars) ?? "not described"))
            if let ic = clip(c.interfaceCompleteness, 40) { parts.append("interface: \(ic)") }
            if let e = clip(c.effects, 80) { parts.append("effects: \(e)") }
            if let h = clip(c.helpExcerpt, Limits.maxHelpChars) { parts.append("help: \(h)") }
            lines.append(parts.joined(separator: " | "))
        }
        return lines.joined(separator: "\n")
    }

    static func expandPrompt(_ r: BridgeRequest) -> String {
        "Intent: \(clip(r.intent, Limits.maxIntentChars) ?? "")"
    }
}

// MARK: - Resolver

struct Resolver {
    var backend: ModelBackend

    static let relevances: Set<String> = ["high", "medium", "low", "none"]

    func handle(_ request: BridgeRequest) async -> BridgeResponse {
        let started = Date()
        var response = await dispatch(request)
        response.latencyMs = Int(Date().timeIntervalSince(started) * 1000)
        return response
    }

    private func dispatch(_ request: BridgeRequest) async -> BridgeResponse {
        let avail = backend.availability()
        if request.op == "status" {
            return BridgeResponse(
                requestId: request.requestId,
                status: avail.state == "available" ? "available" : "unavailable",
                availability: avail)
        }
        guard avail.state == "available" else {
            var r = BridgeResponse.failure(
                request.requestId,
                BridgeFailure(code: "model_unavailable", message: "Foundation Models is not available: \(avail.state)"),
                status: "unavailable")
            r.availability = avail
            return r
        }
        do {
            switch request.op {
            case "expand":
                let raw = try await backend.expand(
                    instructions: Prompts.expandInstructions, prompt: Prompts.expandPrompt(request))
                var r = BridgeResponse(requestId: request.requestId, status: "expanded")
                r.terms = Self.sanitizeTerms(raw)
                return r
            default:
                let out = try await backend.rank(
                    instructions: Prompts.rankInstructions, prompt: Prompts.rankPrompt(request))
                return try Self.response(for: request, from: out)
            }
        } catch let f as BridgeFailure {
            return BridgeResponse.failure(request.requestId, f)
        } catch {
            return BridgeResponse.failure(
                request.requestId,
                BridgeFailure(code: "inference_failed", message: "\(error)"))
        }
    }

    /// Turn the model's picks into the response contract. Unknown references,
    /// duplicates, and malformed values make the whole result invalid.
    static func response(for request: BridgeRequest, from out: ModelRankingOutput) throws -> BridgeResponse {
        let candidates = request.candidates ?? []
        var seen = Set<Int>()
        var rankings: [BridgeRanking] = []
        for pick in out.picks {
            guard pick.number >= 1, pick.number <= candidates.count else {
                throw BridgeFailure(
                    code: "invalid_model_output",
                    message: "model referred to candidate \(pick.number), which was not supplied")
            }
            guard seen.insert(pick.number).inserted else {
                throw BridgeFailure(
                    code: "invalid_model_output", message: "model ranked candidate \(pick.number) twice")
            }
            guard relevances.contains(pick.relevance) else {
                throw BridgeFailure(
                    code: "invalid_model_output", message: "unknown relevance \(pick.relevance)")
            }
            rankings.append(
                BridgeRanking(
                    capabilityId: candidates[pick.number - 1].id,
                    rank: rankings.count + 1,
                    relevance: pick.relevance,
                    reason: Prompts.clip(pick.reason, Limits.maxFieldChars) ?? ""))
        }
        switch out.outcome {
        case "ranked":
            guard !rankings.isEmpty else {
                throw BridgeFailure(code: "invalid_model_output", message: "status ranked with no rankings")
            }
            return BridgeResponse(requestId: request.requestId, status: "ranked", rankings: rankings)
        case "no_match":
            return BridgeResponse(requestId: request.requestId, status: "no_match", rankings: [])
        case "clarification":
            let q = Prompts.clip(out.clarification, Limits.maxFieldChars)
            guard let q else {
                throw BridgeFailure(
                    code: "invalid_model_output", message: "clarification requested without a question")
            }
            return BridgeResponse(
                requestId: request.requestId, status: "clarification", rankings: rankings, clarification: q)
        default:
            throw BridgeFailure(code: "invalid_model_output", message: "unknown outcome \(out.outcome)")
        }
    }

    /// Search terms are matched against the real inventory by the caller, so
    /// they are kept to a conservative alphabet and bounded.
    static func sanitizeTerms(_ raw: [String]) -> [String] {
        var out: [String] = []
        for t in raw {
            let s = t.lowercased().trimmingCharacters(in: .whitespacesAndNewlines)
            guard s.count >= 2, s.count <= 32,
                s.unicodeScalars.allSatisfy({ CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyz0123456789._+-").contains($0) }),
                !out.contains(s)
            else { continue }
            out.append(s)
            if out.count == 10 { break }
        }
        return out
    }
}

// MARK: - Apple Foundation Models backend

#if canImport(FoundationModels)

@available(macOS 26.0, *)
@Generable
enum GenRelevance: String {
    case high, medium, low, none
}

@available(macOS 26.0, *)
@Generable
enum GenOutcome: String {
    case ranked, noMatch, clarification
}

@available(macOS 26.0, *)
@Generable
struct GenPick {
    @Guide(description: "The number shown in square brackets for the candidate", .range(1...40))
    var number: Int
    @Guide(description: "At most 15 words: what this candidate does, and whether that accomplishes the intent")
    var reason: String
    var relevance: GenRelevance
}

@available(macOS 26.0, *)
@Generable
struct GenRanking {
    @Guide(description: "Relevant candidates, most relevant first. Leave empty only if no candidate could plausibly do what the intent asks.", .maximumCount(10))
    var picks: [GenPick]
    var outcome: GenOutcome
    @Guide(description: "A single short question; only for the clarification outcome")
    var clarificationQuestion: String?
}

@available(macOS 26.0, *)
@Generable
struct GenTerms {
    @Guide(description: "Single lowercase words likely to appear in a suitable program's name", .maximumCount(10))
    var terms: [String]
}

@available(macOS 26.0, *)
struct AppleModelBackend: ModelBackend {
    private var model: SystemLanguageModel { SystemLanguageModel.default }
    private let options = GenerationOptions(samplingMode: .greedy, maximumResponseTokens: 1500)

    func availability() -> BridgeAvailability {
        switch model.availability {
        case .available:
            return BridgeAvailability(state: "available", detail: nil)
        case .unavailable(let reason):
            switch reason {
            case .deviceNotEligible:
                return BridgeAvailability(state: "device_not_eligible", detail: "The device is not eligible for Apple Intelligence.")
            case .appleIntelligenceNotEnabled:
                return BridgeAvailability(state: "apple_intelligence_not_enabled", detail: "Apple Intelligence is not enabled in System Settings.")
            case .modelNotReady:
                return BridgeAvailability(state: "model_not_ready", detail: "The model is not ready yet (it may still be downloading).")
            @unknown default:
                return BridgeAvailability(state: "unavailable_unknown", detail: "The framework reported the model unavailable for an unrecognized reason: \(reason).")
            }
        }
    }

    func rank(instructions: String, prompt: String) async throws -> ModelRankingOutput {
        do {
            let r = try await Self.withOneRetry {
                try await LanguageModelSession(model: model, instructions: instructions)
                    .respond(to: prompt, generating: GenRanking.self, options: options)
            }
            let c = r.content
            let outcome: String
            switch c.outcome {
            case .ranked: outcome = "ranked"
            case .noMatch: outcome = "no_match"
            case .clarification: outcome = "clarification"
            }
            return ModelRankingOutput(
                outcome: outcome,
                picks: c.picks.map { ModelPick(number: $0.number, relevance: $0.relevance.rawValue, reason: $0.reason) },
                clarification: c.clarificationQuestion)
        } catch let e as LanguageModelSession.GenerationError {
            throw Self.map(e)
        }
    }

    func expand(instructions: String, prompt: String) async throws -> [String] {
        do {
            return try await Self.withOneRetry {
                try await LanguageModelSession(model: model, instructions: instructions)
                    .respond(to: prompt, generating: GenTerms.self, options: options)
            }.content.terms
        } catch let e as LanguageModelSession.GenerationError {
            throw Self.map(e)
        }
    }

    /// Greedy decoding is deterministic, so a retry only helps when the first
    /// attempt failed to decode; context and guardrail errors are not retried.
    static func withOneRetry<T>(_ body: () async throws -> T) async throws -> T {
        do { return try await body() }
        catch let e as LanguageModelSession.GenerationError {
            switch e {
            case .exceededContextWindowSize, .guardrailViolation, .assetsUnavailable: throw e
            default: return try await body()
            }
        }
    }

    static func map(_ e: LanguageModelSession.GenerationError) -> BridgeFailure {
        switch e {
        case .exceededContextWindowSize:
            return BridgeFailure(code: "context_exceeded", message: "the request exceeded the model's context window")
        case .guardrailViolation:
            return BridgeFailure(code: "guardrail_violation", message: "the model's safety guardrails declined this request")
        case .assetsUnavailable:
            return BridgeFailure(code: "model_unavailable", message: "model assets are unavailable")
        default:
            return BridgeFailure(code: "inference_failed", message: "\(e)")
        }
    }
}

func makeDefaultBackend() -> ModelBackend {
    if #available(macOS 26.0, *) { return AppleModelBackend() }
    return UnsupportedBackend()
}

#else

func makeDefaultBackend() -> ModelBackend { UnsupportedBackend() }

#endif

/// Used when the FoundationModels framework is not part of the build SDK or OS.
struct UnsupportedBackend: ModelBackend {
    func availability() -> BridgeAvailability {
        BridgeAvailability(state: "unsupported_platform", detail: "FoundationModels requires macOS 26 or later and an SDK that provides it.")
    }
    func rank(instructions: String, prompt: String) async throws -> ModelRankingOutput {
        throw BridgeFailure(code: "model_unavailable", message: "unsupported platform")
    }
    func expand(instructions: String, prompt: String) async throws -> [String] {
        throw BridgeFailure(code: "model_unavailable", message: "unsupported platform")
    }
}
