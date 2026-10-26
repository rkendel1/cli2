// Writes dist/bundle.js from src/index.js.
const fs = require("fs");
fs.mkdirSync("dist", { recursive: true });
fs.writeFileSync("dist/bundle.js", "// built\n" + fs.readFileSync("src/index.js", "utf8"));
console.log("wrote dist/bundle.js");
