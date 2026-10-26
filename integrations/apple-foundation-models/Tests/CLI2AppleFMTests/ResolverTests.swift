import XCTest
@testable import CLI2AppleFM

struct ScriptedBackend: ModelBackend {
    var state = "available"
    var output = ModelRankingOutput(outcome: "ranked", picks: [], clarification: nil)
    var terms: [String] = []
    func availability() -> BridgeAvailability { BridgeAvailability(state: state, detail: nil) }
    func rank(instructions: String, prompt: String) async throws -> ModelRankingOutput { output }
    func expand(instructions: String, prompt: String) async throws -> [String] { terms }
}

final class ResolverTests: XCTestCase {
    func request() -> Data {
        Data("""
        {"schema_version":"cli2.apple_fm.request.v1","request_id":"r1","intent":"find text",
         "constraints":{"modify_files":false,"network":"unknown"},
         "candidates":[{"id":"executable:rg","name":"rg","purpose":"search"},
                       {"id":"project-script:npm:test","name":"test"}]}
        """.utf8)
    }

    func testGeneratedStructureBecomesTypedResponse() async throws {
        let req = try RequestDecoder.decode(request())
        let backend = ScriptedBackend(output: ModelRankingOutput(
            outcome: "ranked",
            picks: [ModelPick(number: 2, relevance: "low", reason: "tests"), ModelPick(number: 1, relevance: "high", reason: "search")],
            clarification: nil))
        let r = await Resolver(backend: backend).handle(req)
        XCTAssertEqual(r.status, "ranked")
        XCTAssertEqual(r.rankings.map(\.capabilityId), ["project-script:npm:test", "executable:rg"])
        XCTAssertEqual(r.rankings.map(\.rank), [1, 2])
        let json = String(decoding: try r.json(), as: UTF8.self)
        XCTAssertTrue(json.contains("\"clarification\":null"))
        XCTAssertTrue(json.contains("\"capability_id\""))
    }

    func testMalformedRequestsAreRejected() {
        let bads = ["not json", "{}",
            #"{"schema_version":"v0","request_id":"x","intent":"a","candidates":[{"id":"a","name":"a"}]}"#,
            #"{"schema_version":"cli2.apple_fm.request.v1","request_id":"x","intent":"a","candidates":[]}"#,
            #"{"schema_version":"cli2.apple_fm.request.v1","request_id":"x","intent":"a","candidates":[{"id":"a","name":"a"},{"id":"a","name":"b"}]}"#]
        for bad in bads {
            XCTAssertThrowsError(try RequestDecoder.decode(Data(bad.utf8)), bad)
        }
        XCTAssertThrowsError(try RequestDecoder.decode(Data(repeating: 32, count: Limits.maxRequestBytes + 1)))
    }

    func testUnavailableStatesAreReportedNotHidden() async throws {
        for state in ["device_not_eligible", "apple_intelligence_not_enabled", "model_not_ready", "unavailable_unknown"] {
            let r = await Resolver(backend: ScriptedBackend(state: state)).handle(try RequestDecoder.decode(request()))
            XCTAssertEqual(r.status, "unavailable")
            XCTAssertEqual(r.availability?.state, state)
            XCTAssertTrue(r.rankings.isEmpty)
            XCTAssertEqual(r.error?.code, "model_unavailable")
        }
    }

    func testInvalidCandidateReferencesCannotPass() async throws {
        let req = try RequestDecoder.decode(request())
        let cases: [[ModelPick]] = [
            [ModelPick(number: 3, relevance: "high", reason: "")],
            [ModelPick(number: 0, relevance: "high", reason: "")],
            [ModelPick(number: 1, relevance: "high", reason: ""), ModelPick(number: 1, relevance: "low", reason: "")],
            [ModelPick(number: 1, relevance: "certain", reason: "")]]
        for picks in cases {
            let backend = ScriptedBackend(output: ModelRankingOutput(outcome: "ranked", picks: picks, clarification: nil))
            let r = await Resolver(backend: backend).handle(req)
            XCTAssertEqual(r.status, "error")
            XCTAssertEqual(r.error?.code, "invalid_model_output")
        }
    }

    func testTermsAreSanitized() async throws {
        let req = try RequestDecoder.decode(Data(#"{"schema_version":"cli2.apple_fm.request.v1","request_id":"x","operation":"expand","intent":"a"}"#.utf8))
        let r = await Resolver(backend: ScriptedBackend(terms: ["Grep", "rm -rf /", "rg", "x"])).handle(req)
        XCTAssertEqual(r.terms, ["grep", "rg"])
    }

    func testProtocolGoesToStdoutAndDiagnosticsToStderr() throws {
        let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
        let candidates = [".build/debug/cli2-apple-fm", ".build/release/cli2-apple-fm"].map { root.appendingPathComponent($0) }
        guard let exe = candidates.first(where: { FileManager.default.isExecutableFile(atPath: $0.path) }) else {
            throw XCTSkip("executable not built")
        }
        let p = Process(); p.executableURL = exe
        let i = Pipe(), o = Pipe(), e = Pipe()
        p.standardInput = i; p.standardOutput = o; p.standardError = e
        try p.run()
        i.fileHandleForWriting.write(Data("garbage".utf8)); try i.fileHandleForWriting.close()
        p.waitUntilExit()
        let out = o.fileHandleForReading.readDataToEndOfFile()
        let err = String(decoding: e.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
        let obj = try JSONSerialization.jsonObject(with: out) as? [String: Any]
        XCTAssertEqual(obj?["status"] as? String, "error")
        XCTAssertTrue(err.contains("malformed_request"))
        XCTAssertEqual(out.filter { $0 == 0x0A }.count, 1, "exactly one line on stdout")
    }
}
