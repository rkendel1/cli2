// Minimal test runner for the fixture: no dependencies, no network.
const assert = require("assert");
const fail = process.argv.includes("--fail");
const results = [];
function test(name, fn) {
  try { fn(); results.push(["ok", name]); } catch (e) { results.push(["not ok", name, e.message]); }
}
test("addition", () => assert.strictEqual(1 + 1, 2));
test("strings", () => assert.strictEqual("a" + "b", "ab"));
if (fail) test("deliberate failure", () => assert.strictEqual(1, 2));
for (const [s, n, m] of results) console.log(`${s} - ${n}${m ? ": " + m : ""}`);
const failed = results.filter(r => r[0] !== "ok").length;
console.log(`# tests ${results.length}, pass ${results.length - failed}, fail ${failed}`);
process.exit(failed ? 1 : 0);
