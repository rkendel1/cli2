// Authentication helpers.
function authenticate(user, password) {
  return user === "admin" && password === "secret";
}
module.exports = { authenticate };
