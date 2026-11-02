//! Who is talking: the application built on this core.
//!
//! EVE's servers, the Jabber server and the person reading a message should all
//! be told the truth about which program they are dealing with, and Waggle and
//! EVEmarchy each ask for a different set of ESI scopes. Everything here that
//! names itself or checks scopes takes one of these.

/// A front end's name, version and the promises its logins carry.
#[derive(Debug, Clone, Copy)]
pub struct AppIdentity {
    /// Shown to people and sent to servers, such as `Waggle`.
    pub name: &'static str,
    pub version: &'static str,
    /// Where the program lives, for the User-Agent ESI asks every client to send.
    pub website: &'static str,
    /// Scopes every linked character must have granted. A token without one of
    /// these is refused at login and at every refresh.
    pub required_scopes: &'static [&'static str],
    /// Tells a person how to link `character` again; the caller appends what
    /// that would unlock, such as "to see skill training".
    pub relink: fn(character: &str) -> String,
}

impl AppIdentity {
    /// `Name/version (website)`, the User-Agent shape ESI's guidelines ask for.
    pub fn user_agent(&self) -> String {
        format!("{}/{} ({})", self.name, self.version, self.website)
    }
}

#[cfg(test)]
pub(crate) const TEST_APP: AppIdentity = AppIdentity {
    name: "TestApp",
    version: "0.0.0",
    website: "https://example.invalid/testapp",
    required_scopes: &[
        "esi-location.read_online.v1",
        "esi-location.read_location.v1",
        "esi-industry.read_character_jobs.v1",
    ],
    relink: |character| format!("Run testapp auth add for {character} again"),
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_names_the_program_and_where_it_lives() {
        assert_eq!(
            TEST_APP.user_agent(),
            "TestApp/0.0.0 (https://example.invalid/testapp)"
        );
    }
}
