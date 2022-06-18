// TODO: client should create project then push new deployment (refactor endpoint)
// TODO: Add some tests (ideas?)
// TODO: Implement the delete project endpoint to make sure users can
//       self-serve out of issues

#![allow(warnings)]

#[macro_use]
extern crate async_trait;

#[macro_use]
extern crate log;
extern crate core;

use std::convert::Infallible;
use std::error::Error as StdError;
use std::fmt::Formatter;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use bollard::Docker;
use clap::Parser;
use convert_case::{Case, Casing};
use futures::prelude::*;
use futures::stream::TryUnfold;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;

macro_rules! value_block_helper {
    ($next:ident, $block:block) => {
        $block
    };
    ($next:ident,) => {
        $next
    };
}

#[macro_export]
macro_rules! assert_stream_matches {
    (
        $stream:ident,
        $(#[assertion = $assert:literal])?
        $($pattern:pat_param)|+ $(if $guard:expr)? $(=> $more:block)?,
    ) => {{
        let next = ::futures::stream::StreamExt::next(&mut $stream)
            .await
            .expect("Stream ended before the last of assertions");

        match &next {
            $($pattern)|+ $(if $guard)? => {
                print!("{}", ::colored::Colorize::green(::colored::Colorize::bold("[ok]")));
                $(print!(" {}", $assert);)?
                print!("\n");
                value_block_helper!(next, $($more)?)
            },
            _ => {
                eprintln!("{} {:#?}", ::colored::Colorize::red(::colored::Colorize::bold("[err]")), next);
                eprint!("{}", ::colored::Colorize::red(::colored::Colorize::bold("Assertion failed")));
                $(eprint!(": {}", $assert);)?;
                eprint!("\n");
                panic!("State mismatch")
            }
        }
    }};
    (
        $stream:ident,
        $(#[$($meta:tt)*])*
        $($pattern:pat_param)|+ $(if $guard:expr)? $(=> $more:block)?,
        $($(#[$($metas:tt)*])* $($patterns:pat_param)|+ $(if $guards:expr)? $(=> $mores:block)?,)+
    ) => {{
        assert_stream_matches!(
            $stream,
            $(#[$($meta)*])* $($pattern)|+ $(if $guard)? => {
                $($more)?;
                assert_stream_matches!(
                    $stream,
                    $($(#[$($metas)*])* $($patterns)|+ $(if $guards)? $(=> $mores)?,)+
                )
            },
        )
    }};
}

#[macro_export]
macro_rules! assert_matches {
    {
        $ctx:ident,
        $state:expr,
        $($(#[$($meta:tt)*])* $($patterns:pat_param)|+ $(if $guards:expr)? $(=> $mores:block)?,)+
    } => {{
        let state = $state;
        let mut stream = crate::EndStateExt::into_stream(state, $ctx);
        assert_stream_matches!(
            stream,
            $($(#[$($meta)*])* $($patterns)|+ $(if $guards)? $(=> $mores)?,)+
        )
    }}
}

#[macro_export]
macro_rules! assert_err_kind {
    {
        $left:expr, ErrorKind::$right:ident
    } => {{
        let left: Result<_, Error> = $left;
        assert_eq!(
            left.map_err(|err| err.kind()),
            Err(ErrorKind::$right)
        );
    }};
}

use crate::api::make_api;
use crate::args::Args;
use crate::proxy::make_proxy;
use crate::service::GatewayService;
use crate::worker::Worker;

pub mod api;
pub mod args;
pub mod auth;
pub mod project;
pub mod proxy;
pub mod service;
pub mod worker;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    KeyMissing,
    BadHost,
    KeyMalformed,
    Unauthorized,
    Forbidden,
    UserNotFound,
    UserAlreadyExists,
    ProjectNotFound,
    InvalidProjectName,
    ProjectAlreadyExists,
    ProjectNotReady,
    ProjectUnavailable,
    InvalidOperation,
    Internal,
    NotReady,
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let formatted = format!("{:?}", self).to_case(Case::Snake);
        write!(f, "{}", formatted)
    }
}

/// Server-side errors that do not have to do with the user runtime
/// should be [`Error`]s.
///
/// All [`Error`] have an [`ErrorKind`] and an (optional) source.

/// [`Error] is safe to be used as error variants to axum endpoints
/// return types as their [`IntoResponse`] implementation does not
/// leak any sensitive information.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    source: Option<Box<dyn StdError + Sync + Send + 'static>>,
}

impl Error {
    pub fn source<E: StdError + Sync + Send + 'static>(kind: ErrorKind, err: E) -> Self {
        Self {
            kind,
            source: Some(Box::new(err)),
        }
    }

    pub fn custom<S: AsRef<str>>(kind: ErrorKind, message: S) -> Self {
        Self {
            kind,
            source: Some(Box::new(io::Error::new(
                io::ErrorKind::Other,
                message.as_ref().to_string(),
            ))),
        }
    }

    pub fn from_kind(kind: ErrorKind) -> Self {
        Self { kind, source: None }
    }

    fn kind(&self) -> ErrorKind {
        self.kind
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Self::from_kind(kind)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let (status, error_message) = match self.kind {
            ErrorKind::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
            ErrorKind::KeyMissing => (StatusCode::UNAUTHORIZED, "request is missing a key"),
            ErrorKind::KeyMalformed => (StatusCode::BAD_REQUEST, "request has an invalid key"),
            ErrorKind::BadHost => (StatusCode::BAD_REQUEST, "the 'Host' header is invalid"),
            ErrorKind::UserNotFound => (StatusCode::NOT_FOUND, "user not found"),
            ErrorKind::UserAlreadyExists => (StatusCode::BAD_REQUEST, "user already exists"),
            ErrorKind::ProjectNotFound => (StatusCode::NOT_FOUND, "project not found"),
            ErrorKind::ProjectNotReady => (StatusCode::SERVICE_UNAVAILABLE, "project not ready"),
            ErrorKind::ProjectUnavailable => {
                (StatusCode::BAD_GATEWAY, "project returned invalid response")
            }
            ErrorKind::InvalidProjectName => (StatusCode::BAD_REQUEST, "invalid project name"),
            ErrorKind::InvalidOperation => (
                StatusCode::BAD_REQUEST,
                "the requested operation is invalid",
            ),
            ErrorKind::ProjectAlreadyExists => (
                StatusCode::BAD_REQUEST,
                "a project with the same name already exists",
            ),
            ErrorKind::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ErrorKind::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ErrorKind::NotReady => (StatusCode::INTERNAL_SERVER_ERROR, "not ready yet"),
        };
        (status, Json(json!({ "error": error_message }))).into_response()
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind)?;
        if let Some(source) = self.source.as_ref() {
            write!(f, ": ")?;
            source.fmt(f)?;
        }
        Ok(())
    }
}

impl StdError for Error {}

#[derive(Debug, sqlx::Type, Serialize, Clone, PartialEq, Eq)]
#[sqlx(transparent)]
pub struct ProjectName(pub String);

impl<'de> Deserialize<'de> for ProjectName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(|_err| todo!())
    }
}

impl FromStr for ProjectName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let re = regex::Regex::new("^[a-zA-Z0-9\\-_]{3,64}$").unwrap();
        if re.is_match(s) {
            Ok(Self(s.to_string()))
        } else {
            Err(Error::from_kind(ErrorKind::InvalidProjectName))
        }
    }
}

impl std::fmt::Display for ProjectName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type, Serialize)]
#[sqlx(transparent)]
pub struct AccountName(pub String);

impl FromStr for AccountName {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.to_string()))
    }
}

impl std::fmt::Display for AccountName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<'de> Deserialize<'de> for AccountName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(|_err| todo!())
    }
}

pub trait Context<'c>: Send + Sync {
    fn docker(&self) -> &'c Docker;

    fn args(&self) -> &'c Args;
}

#[async_trait]
pub trait Service<'c> {
    type Context: Context<'c>;

    type State: EndState<'c>;

    type Error;

    /// Asks for the latest available context for task execution
    fn context(&'c self) -> Self::Context;

    /// Commit a state update to persistence
    async fn update(&mut self, state: &Self::State) -> Result<(), Self::Error>;
}

/// A generic state which can, when provided with a [`Context`], do
/// some work and advance itself
#[async_trait]
pub trait State<'c>: Send + Sized + Clone {
    type Next;

    type Error;

    async fn next<C: Context<'c>>(self, ctx: &C) -> Result<Self::Next, Self::Error>;
}

/// A [`State`] which contains all its transitions, including
/// failures
pub trait EndState<'c>
where
    Self: State<'c, Error = Infallible, Next = Self>,
{
    type ErrorVariant;

    fn is_done(&self) -> bool;

    fn into_result(self) -> Result<Self, Self::ErrorVariant>;
}

pub type StateTryStream<'c, St: EndState<'c>> =
    Pin<Box<dyn Stream<Item = Result<St, St::ErrorVariant>> + Send + 'c>>;

pub trait EndStateExt<'c>: EndState<'c> {
    /// Convert the state into a [`TryStream`] that yields
    /// the generated states.
    ///
    /// This stream will not end.
    fn into_stream<Ctx>(self, ctx: Ctx) -> StateTryStream<'c, Self>
    where
        Self: 'c,
        Ctx: 'c + Context<'c>,
    {
        Box::pin(stream::try_unfold((self, ctx), |(state, ctx)| async move {
            state
                .next(&ctx)
                .await
                .unwrap() // EndState's `next` is Infallible
                .into_result()
                .map(|state| Some((state.clone(), (state, ctx))))
        }))
    }
}

impl<'c, S> EndStateExt<'c> for S where S: EndState<'c> {}

pub trait IntoEndState<'c, E>
where
    E: EndState<'c>,
{
    fn into_end_state(self) -> Result<E, Infallible>;
}

impl<'c, E, S, Err> IntoEndState<'c, E> for Result<S, Err>
where
    E: EndState<'c> + From<S> + From<Err>,
{
    fn into_end_state(self) -> Result<E, Infallible> {
        self.map(|s| E::from(s)).or_else(|err| Ok(E::from(err)))
    }
}

#[async_trait]
pub trait Refresh: Sized {
    type Error: StdError;

    async fn refresh<'c, C: Context<'c>>(self, ctx: &C) -> Result<Self, Self::Error>;
}

#[tokio::main]
async fn main() -> io::Result<()> {
    env_logger::init();

    let args = Args::parse();

    let gateway = Arc::new(GatewayService::init(args.clone()).await);

    let worker = Worker::new(Arc::clone(&gateway));
    gateway.set_sender(Some(worker.sender())).await;
    gateway
        .refresh()
        .map_err(|err| error!("failed to refresh state at startup: {err}"))
        .await
        .unwrap();

    let worker_handle = tokio::spawn(
        worker
            .start()
            .map_ok(|_| info!("worker terminated successfully"))
            .map_err(|err| error!("worker error: {}", err)),
    );

    let api = make_api(Arc::clone(&gateway));

    let api_handle = tokio::spawn(axum::Server::bind(&args.control).serve(api.into_make_service()));

    let proxy = make_proxy(gateway);

    let proxy_handle = tokio::spawn(hyper::Server::bind(&args.user).serve(proxy));

    tokio::join!(worker_handle, api_handle, proxy_handle);

    Ok(())
}

#[cfg(test)]
pub mod tests {
    use std::env;

    use anyhow::{anyhow, Context as AnyhowContext};
    use bollard::{models::Health, network::ListNetworksOptions, Docker};
    use colored::Colorize;
    use rand::distributions::{Alphanumeric, DistString, Distribution, Uniform};
    use tempfile::NamedTempFile;

    use crate::{args::Args, Context};

    use std::net::SocketAddr;

    use serde::Deserialize;

    use http::{
        uri::{PathAndQuery, Scheme, Uri},
        Error as HttpError,
    };
    use hyper::{client::HttpConnector, Body, Client as HyperClient, StatusCode};

    use super::*;

    pub struct Client {
        target: SocketAddr,
        hyper: HyperClient<HttpConnector, Body>,
    }

    impl Client {
        pub fn new(from: &HyperClient<HttpConnector, Body>, target: SocketAddr) -> Self {
            Self {
                target,
                hyper: from.clone(),
            }
        }

        pub async fn get<T, P>(&self, p_and_q: P) -> Result<Option<T>, Error>
        where
            T: for<'de> Deserialize<'de>,
            PathAndQuery: TryFrom<P>,
            <PathAndQuery as TryFrom<P>>::Error: Into<HttpError>,
        {
            let uri = Uri::builder()
                .scheme(Scheme::HTTP)
                .authority(self.target.to_string())
                .path_and_query(p_and_q)
                .build()
                .map_err(|err| Error::source(ErrorKind::Internal, err))?;
            let resp = self.hyper.get(uri).await.unwrap();
            if resp.status() != StatusCode::OK {
                Err(Error::custom(
                    ErrorKind::Internal,
                    format!("invalid response code from runtime: {}", resp.status()),
                ))
            } else {
                let body = resp
                    .into_body()
                    .try_fold(Vec::new(), |mut buf, bytes| async move {
                        buf.extend(bytes.into_iter());
                        Ok(buf)
                    })
                    .await
                    .unwrap();
                if body.is_empty() {
                    Ok(None)
                } else {
                    let t = serde_json::from_slice(&body).unwrap();
                    Ok(t)
                }
            }
        }
    }

    pub struct World {
        state: NamedTempFile,
        docker: Docker,
        args: Args,
        hyper: HyperClient<HttpConnector, Body>,
    }

    #[derive(Clone, Copy)]
    pub struct WorldContext<'c> {
        pub docker: &'c Docker,
        pub args: &'c Args,
        pub hyper: &'c HyperClient<HttpConnector, Body>,
    }

    impl World {
        pub async fn new() -> anyhow::Result<Self> {
            let docker_host =
                option_env!("SHUTTLE_TESTS_DOCKER_HOST").unwrap_or("tcp://127.0.0.1:2735");
            let docker = Docker::connect_with_http_defaults()?;

            docker.list_images::<&str>(None).await.context(anyhow!(
                "A docker daemon does not seem accessible at {docker_host}"
            ))?;

            let control: i16 = Uniform::from(9000..10000).sample(&mut rand::thread_rng());
            let user = control + 1;
            let control = format!("127.0.0.1:{control}").parse().unwrap();
            let user = format!("127.0.0.1:{user}").parse().unwrap();

            let prefix = format!(
                "shuttle_test_{}_",
                Alphanumeric.sample_string(&mut rand::thread_rng(), 4)
            );

            let image = env::var("SHUTTLE_TESTS_RUNTIME_IMAGE")
                .unwrap_or("public.ecr.aws/shuttle/backend:latest".to_string());

            let provisioner_host = env::var("SHUTTLE_TESTS_PROVISIONER_HOST")
                .context("the tests can't run if `SHUTTLE_TESTS_PROVISIONER_HOST` is not set")?;

            let network_id = env::var("SHUTTLE_TESTS_NETWORK_ID")
                .context("the tests can't run if `SHUTTLE_TESTS_NETWORK_ID` is not set")?;

            docker
                .list_networks(Some(ListNetworksOptions {
                    filters: vec![("id", vec![network_id.as_str()])]
                        .into_iter()
                        .collect(),
                }))
                .await
                .context("can't list docker networks")
                .and_then(|networks| {
                    if networks.is_empty() {
                        Err(anyhow!("can't find a docker network with id={network_id}"))
                    } else {
                        Ok(())
                    }
                })?;

            let state = NamedTempFile::new().unwrap();

            let args = Args {
                control,
                user,
                image,
                prefix,
                provisioner_host,
                network_id,
                state: state.path().to_str().unwrap().to_string(),
            };

            let hyper = HyperClient::builder().build(HttpConnector::new());

            Ok(Self {
                state,
                docker,
                args,
                hyper,
            })
        }
    }

    impl World {
        pub fn context<'c>(&'c self) -> WorldContext<'c> {
            WorldContext {
                docker: &self.docker,
                args: &self.args,
                hyper: &self.hyper,
            }
        }
    }

    impl<'c> Context<'c> for WorldContext<'c> {
        fn docker(&self) -> &'c Docker {
            &self.docker
        }

        fn args(&self) -> &'c Args {
            &self.args
        }
    }
}
