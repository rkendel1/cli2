// Stand-in for a real publish step. It never contacts a registry; it records
// that it ran so tests can prove whether it executed.
const fs = require("fs");
fs.writeFileSync("PUBLISHED", new Date().toISOString() + "\n");
console.log("pretend-published cli2-node-fixture@1.0.0");
