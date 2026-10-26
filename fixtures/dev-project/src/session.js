const { authenticate } = require("./auth");
function startSession(user, password) {
  if (!authenticate(user, password)) throw new Error("denied");
  return { user };
}
module.exports = { startSession };
