use std::fmt::Debug;
use std::sync::Arc;

use async_trait::async_trait;
use futures::{Sink, SinkExt};
use tokio::net::TcpListener;

use pgwire::api::auth::noop::NoopStartupHandler;
use pgwire::api::copy::CopyHandler;
use pgwire::api::query::{PlaceholderExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{CopyResponse, Response};
use pgwire::api::{ClientInfo, PgWireHandlerFactory};
use pgwire::error::ErrorInfo;
use pgwire::error::{PgWireError, PgWireResult};
use pgwire::messages::copy::{CopyData, CopyDone, CopyFail};
use pgwire::messages::response::NoticeResponse;
use pgwire::messages::PgWireBackendMessage;
use pgwire::tokio::process_socket;

pub struct DummyProcessor;

#[async_trait]
impl SimpleQueryHandler for DummyProcessor {
    async fn do_query<'a, C>(
        &self,
        client: &mut C,
        query: &'a str,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        client
            .send(PgWireBackendMessage::NoticeResponse(NoticeResponse::from(
                ErrorInfo::new(
                    "NOTICE".to_owned(),
                    "01000".to_owned(),
                    format!("Query received {}", query),
                ),
            )))
            .await?;

        Ok(vec![Response::CopyIn(CopyResponse::new(0, 1, vec![0]))])
    }
}

#[async_trait]
impl CopyHandler for DummyProcessor {
    async fn on_copy_data<C>(&self, _client: &mut C, copy_data: CopyData) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        println!("receiving data: {:?}", copy_data);
        Ok(())
    }

    async fn on_copy_done<C>(&self, _client: &mut C, _done: CopyDone) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        println!("copy done");
        Ok(())
    }

    async fn on_copy_fail<C>(&self, _client: &mut C, fail: CopyFail) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        println!("copy failed: {:?}", fail);
        Ok(())
    }
}

struct DummyProcessorFactory {
    handler: Arc<DummyProcessor>,
}

impl PgWireHandlerFactory for DummyProcessorFactory {
    type StartupHandler = NoopStartupHandler;
    type SimpleQueryHandler = DummyProcessor;
    type ExtendedQueryHandler = PlaceholderExtendedQueryHandler;
    type CopyHandler = DummyProcessor;

    fn simple_query_handler(&self) -> Arc<Self::SimpleQueryHandler> {
        self.handler.clone()
    }

    fn extended_query_handler(&self) -> Arc<Self::ExtendedQueryHandler> {
        Arc::new(PlaceholderExtendedQueryHandler)
    }

    fn startup_handler(&self) -> Arc<Self::StartupHandler> {
        Arc::new(NoopStartupHandler)
    }

    fn copy_handler(&self) -> Arc<Self::CopyHandler> {
        self.handler.clone()
    }
}

#[tokio::main]
pub async fn main() {
    let factory = Arc::new(DummyProcessorFactory {
        handler: Arc::new(DummyProcessor),
    });

    let server_addr = "127.0.0.1:5432";
    let listener = TcpListener::bind(server_addr).await.unwrap();
    println!("Listening to {}", server_addr);
    loop {
        let incoming_socket = listener.accept().await.unwrap();
        let factory_ref = factory.clone();
        tokio::spawn(async move { process_socket(incoming_socket.0, None, factory_ref).await });
    }
}
