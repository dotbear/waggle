/// Waggle's EVE SSO application: Authorization Code with PKCE, no secret.
///
/// Registered 2026-11-02 with the five read scopes listed in PLAN.md §6. A
/// client ID is public by design; every copy of Waggle carries it, so nobody
/// has to register an application of their own.
#[expect(dead_code, reason = "used once the SSO flow lands in the app")]
const EVE_CLIENT_ID: &str = "53f06eddd7454cd4bdd7e8651499199d";

/// Must match the callback registered for [`EVE_CLIENT_ID`] exactly. One port
/// above EVEmarchy's 17632 and away from RIFT's 25252, so all three can run
/// side by side.
#[expect(dead_code, reason = "used once the SSO flow lands in the app")]
const EVE_CALLBACK: &str = "http://127.0.0.1:17633/callback";

fn main() {
    println!("Waggle {}", waggle_core::VERSION);
}
