pub mod bot;
pub mod config;
pub mod pricing;
pub mod state;

mod discovery;
mod orders;

pub(crate) type PublicClient = kuest_client_sdk::clob::Client;
pub(crate) type AuthClient = kuest_client_sdk::clob::Client<
    kuest_client_sdk::auth::state::Authenticated<kuest_client_sdk::auth::Normal>,
>;
