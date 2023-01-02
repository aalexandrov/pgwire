use std::borrow::Cow;
use std::fmt::Debug;
use std::num::NonZeroU32;
use std::ops::BitXor;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use bytes::Bytes;
use futures::{Sink, SinkExt};
use ring::digest;
use ring::hmac;
use ring::pbkdf2;

use crate::api::auth::METADATA_USER;
use crate::api::{ClientInfo, MakeHandler, PgWireConnectionState};
use crate::error::{PgWireError, PgWireResult};
use crate::messages::startup::Authentication;
use crate::messages::{PgWireBackendMessage, PgWireFrontendMessage};

use super::{ServerParameterProvider, StartupHandler};

#[derive(Debug)]
pub enum ScramState {
    Initial,
    // cache salt, channel_binding and partial auth-message
    ServerFirstSent(Vec<u8>, String, String),
}

#[derive(Debug)]
pub struct SASLScramAuthStartupHandler<A, P> {
    auth_db: Arc<A>,
    parameter_provider: Arc<P>,
    /// state of the client-server communication
    state: Mutex<ScramState>,
}

impl<A, P> SASLScramAuthStartupHandler<A, P> {
    pub fn new(auth_db: Arc<A>, parameter_provider: Arc<P>) -> SASLScramAuthStartupHandler<A, P> {
        SASLScramAuthStartupHandler {
            auth_db,
            parameter_provider,
            state: Mutex::new(ScramState::Initial),
        }
    }
}

/// This trait abstracts an authentication database for SCRAM authentication
/// mechanism.
#[async_trait]
pub trait AuthDB: Send + Sync {
    /// Fetch password and add salt, this step is defined in
    /// [RFC5802](https://www.rfc-editor.org/rfc/rfc5802#section-3)
    ///
    /// ```text
    /// SaltedPassword  := Hi(Normalize(password), salt, i)
    /// ```
    ///
    /// The implementation should first retrieve password from its storage and
    /// compute it into SaltedPassword
    async fn get_salted_password(
        &self,
        username: &str,
        salt: &[u8],
        iterations: usize,
    ) -> PgWireResult<Vec<u8>>;
}

/// compute salted password from raw password
pub fn gen_salted_password(password: &str, salt: &[u8], iters: usize) -> Vec<u8> {
    // according to postgres doc, if we failed to normalize password, use
    // original password instead of throwing error
    let normalized_pass = stringprep::saslprep(password).unwrap_or(Cow::Borrowed(password));
    let pass_bytes = normalized_pass.as_ref().as_bytes();
    hi(pass_bytes, salt, iters)
}

pub fn random_salt() -> Vec<u8> {
    let mut buf = vec![0u8; 10];
    for v in buf.iter_mut() {
        *v = rand::random::<u8>();
    }
    buf
}

pub fn random_nonce() -> String {
    let mut buf = [0u8; 18];
    for v in buf.iter_mut() {
        *v = rand::random::<u8>();
    }

    STANDARD.encode(buf)
}

const DEFAULT_ITERATIONS: usize = 4096;

#[async_trait]
impl<A: AuthDB, P: ServerParameterProvider> StartupHandler for SASLScramAuthStartupHandler<A, P> {
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        match message {
            PgWireFrontendMessage::Startup(ref startup) => {
                super::save_startup_parameters_to_metadata(client, startup);
                client.set_state(PgWireConnectionState::AuthenticationInProgress);
                client
                    .send(PgWireBackendMessage::Authentication(Authentication::SASL(
                        vec!["SCRAM-SHA-256".to_owned(), "SCRAM-SHA-256-PLUS".to_owned()],
                    )))
                    .await?;
            }
            PgWireFrontendMessage::PasswordMessageFamily(msg) => {
                let salt = {
                    // this should never block
                    let state0 = self.state.lock().unwrap();
                    if let ScramState::ServerFirstSent(ref salt, _, _) = *state0 {
                        Some(salt.to_vec())
                    } else {
                        None
                    }
                };

                let salted_password = if let Some(ref salt) = salt {
                    let username = client
                        .metadata()
                        .get(METADATA_USER)
                        .ok_or(PgWireError::UserNameRequired)?;
                    Some(
                        self.auth_db
                            .get_salted_password(username, salt, DEFAULT_ITERATIONS)
                            .await?,
                    )
                } else {
                    None
                };

                let mut success = false;
                let resp = {
                    // this should never block
                    let mut state = self.state.lock().unwrap();
                    match *state {
                        ScramState::Initial => {
                            // initial response, client_first
                            let resp = msg.into_sasl_initial_response()?;
                            // parse into client_first
                            let client_first = resp
                                .data()
                                .as_ref()
                                .ok_or_else(|| {
                                    PgWireError::InvalidScramMessage(
                                        "Empty client-first".to_owned(),
                                    )
                                })
                                .and_then(|data| {
                                    ClientFirst::try_new(String::from_utf8_lossy(data).as_ref())
                                })?;
                            dbg!(&client_first);

                            // create server_first and send
                            let mut new_nonce = client_first.nonce.clone();
                            new_nonce.push_str(random_nonce().as_str());

                            let salt = random_salt();
                            let server_first = ServerFirst::new(
                                new_nonce,
                                STANDARD.encode(&salt),
                                DEFAULT_ITERATIONS,
                            );
                            let server_first_message = server_first.message();

                            *state = ScramState::ServerFirstSent(
                                salt,
                                client_first.channel_binding(),
                                format!("{},{}", client_first.bare(), &server_first_message),
                            );
                            Authentication::SASLContinue(Bytes::from(server_first_message))
                        }
                        ScramState::ServerFirstSent(
                            _,
                            ref channel_binding_prefix,
                            ref partial_auth_msg,
                        ) => {
                            // second response, client_final
                            let resp = msg.into_sasl_response()?;
                            let client_final = ClientFinal::try_new(
                                String::from_utf8_lossy(resp.data().as_ref()).as_ref(),
                            )?;
                            dbg!(&client_final);
                            // TODO: add channel binding content
                            client_final
                                .validate_channel_binding(channel_binding_prefix.as_bytes())?;

                            let salted_password = salted_password.unwrap();
                            let client_key = hmac(salted_password.as_ref(), b"Client Key");
                            let stored_key = h(client_key.as_ref());
                            let auth_msg =
                                format!("{},{}", partial_auth_msg, client_final.without_proof());
                            let client_signature = hmac(stored_key.as_ref(), auth_msg.as_bytes());

                            let computed_client_proof = STANDARD.encode(
                                xor(client_key.as_ref(), client_signature.as_ref()).as_slice(),
                            );

                            if computed_client_proof == client_final.proof {
                                let server_key = hmac(salted_password.as_ref(), b"Server Key");
                                let server_signature =
                                    hmac(server_key.as_ref(), auth_msg.as_bytes());
                                let server_final =
                                    ServerFinalSuccess::new(STANDARD.encode(server_signature));
                                success = true;
                                Authentication::SASLFinal(Bytes::from(server_final.message()))
                            } else {
                                let server_final =
                                    ServerFinalError::new("invalid-proof".to_owned());
                                Authentication::SASLFinal(Bytes::from(server_final.message()))
                            }
                        }
                    }
                };

                client
                    .send(PgWireBackendMessage::Authentication(resp))
                    .await?;

                if success {
                    super::finish_authentication(client, self.parameter_provider.as_ref()).await
                }
            }
            _ => {}
        }

        Ok(())
    }
}

#[derive(Debug, new)]
pub struct MakeSASLScramAuthStartupHandler<A, P> {
    auth_db: Arc<A>,
    parameter_provider: Arc<P>,
}

impl<A, P> MakeHandler for MakeSASLScramAuthStartupHandler<A, P> {
    type Handler = Arc<SASLScramAuthStartupHandler<A, P>>;

    fn make(&self) -> Self::Handler {
        Arc::new(SASLScramAuthStartupHandler {
            auth_db: self.auth_db.clone(),
            parameter_provider: self.parameter_provider.clone(),
            state: Mutex::new(ScramState::Initial),
        })
    }
}

#[allow(dead_code)]
#[derive(Debug)]
struct ClientFirst {
    cbind_flag: String,
    auth_zid: String,
    username: String,
    nonce: String,
}

impl ClientFirst {
    fn try_new(s: &str) -> PgWireResult<ClientFirst> {
        let parts: Vec<&str> = s.splitn(4, ',').collect();
        if parts.len() != 4
            || !Self::validate_cbind_flag(parts[0])
            || !parts[2].starts_with("n=")
            || !parts[3].starts_with("r=")
        {
            return Err(PgWireError::InvalidScramMessage(s.to_owned()));
        }
        // now it's safe to unwrap
        let cbind_flag = parts[0].to_owned();
        // add additional check when we don't have channel binding
        // if cbind_flag != 'n' {
        //     return Err(PgWireError::InvalidScramMessage(format!(
        //         "cbing_flag: {}, but channel binding not supported.",
        //         cbind_flag
        //     )));
        // }

        let auth_zid = parts[1].to_owned();
        let username = parts[2].strip_prefix("n=").unwrap().to_owned();
        let nonce = parts[3].strip_prefix("r=").unwrap().to_owned();

        Ok(ClientFirst {
            cbind_flag,
            auth_zid,
            username,
            nonce,
        })
    }

    fn validate_cbind_flag(flag: &str) -> bool {
        flag == "n" || flag == "y" || flag.starts_with("p=")
    }

    fn bare(&self) -> String {
        format!("n={},r={}", self.username, self.nonce)
    }

    fn channel_binding(&self) -> String {
        format!("{},{},", self.cbind_flag, self.auth_zid)
    }

    /// tls channel binding types:
    ///
    /// - `tls-unique`
    /// - `tls-server-end-point`
    fn channel_binding_type(&self) -> Option<&str> {
        self.cbind_flag.strip_prefix("p=")
    }
}

#[derive(Debug, new)]
struct ServerFirst {
    nonce: String,
    salt: String,
    iteration: usize,
}

impl ServerFirst {
    fn message(&self) -> String {
        format!("r={},s={},i={}", self.nonce, self.salt, self.iteration)
    }
}

#[derive(Debug)]
struct ClientFinal {
    channel_binding: String,
    nonce: String,
    proof: String,
}

impl ClientFinal {
    fn try_new(s: &str) -> PgWireResult<ClientFinal> {
        let parts: Vec<&str> = s.splitn(3, ',').collect();
        if parts.len() != 3
            || !parts[0].starts_with("c=")
            || !parts[1].starts_with("r=")
            || !parts[2].starts_with("p=")
        {
            return Err(PgWireError::InvalidScramMessage(s.to_owned()));
        }

        // safe to unwrap after check

        let channel_binding = parts[0].strip_prefix("c=").unwrap().to_owned();
        let nonce = parts[1].strip_prefix("r=").unwrap().to_owned();
        let proof = parts[2].strip_prefix("p=").unwrap().to_owned();

        Ok(ClientFinal {
            channel_binding,
            nonce,
            proof,
        })
    }

    fn validate_channel_binding(&self, channel_binding: &[u8]) -> PgWireResult<()> {
        // TODO: add support for tls-server-end-point channel binding

        // validate channel binding
        let decoded_channel_binding = base64::decode(&self.channel_binding).map_err(|e| {
            PgWireError::InvalidScramMessage(format!(
                "Failed to decode channel binding: {}, {}",
                self.channel_binding, e
            ))
        })?;
        // compare
        if decoded_channel_binding.as_slice() == channel_binding {
            Ok(())
        } else {
            Err(PgWireError::InvalidScramMessage(format!(
                "Channel binding mismatch, expect: {:?}, decoded: {:?}",
                channel_binding,
                decoded_channel_binding.as_slice()
            )))
        }
    }

    fn without_proof(&self) -> String {
        format!("c={},r={}", self.channel_binding, self.nonce)
    }
}

#[derive(Debug, new)]
struct ServerFinalSuccess {
    verifier: String,
}

impl ServerFinalSuccess {
    fn message(&self) -> String {
        format!("v={}", self.verifier)
    }
}

#[derive(Debug, new)]
struct ServerFinalError {
    error: String,
}

impl ServerFinalError {
    fn message(&self) -> String {
        format!("e={}", self.error)
    }
}

fn hi(normalized_password: &[u8], salt: &[u8], iterations: usize) -> Vec<u8> {
    let mut buf = [0u8; 32];

    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(iterations as u32).unwrap(),
        salt,
        normalized_password,
        &mut buf,
    );
    buf.to_vec()
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mac = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&mac, msg).as_ref().to_vec()
}

fn h(msg: &[u8]) -> Vec<u8> {
    digest::digest(&digest::SHA256, msg).as_ref().to_vec()
}

fn xor(lhs: &[u8], rhs: &[u8]) -> Vec<u8> {
    lhs.iter()
        .zip(rhs.iter())
        .map(|(l, r)| l.bitxor(r))
        .collect()
}
