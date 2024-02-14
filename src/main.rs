include!("macros.rs");

pub mod config;
pub mod error;
pub mod globals;
pub mod store;
pub mod tls;
pub mod types;
pub mod web;

use crate::config::Config;
use crate::error::Error;
use crate::globals::GLOBALS;
use crate::store::Store;
use crate::tls::MaybeTlsStream;
use hyper::service::Service;
use hyper::{Body, Request, Response};
use std::env;
use std::error::Error as StdError;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Read;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::net::{TcpListener, TcpStream};

#[tokio::main]
async fn main() -> Result<(), Error> {
    env_logger::init();

    // Get args (config path)
    let mut args = env::args();
    if args.len() <= 1 {
        panic!("USAGE: chorus <config_path>");
    }
    let _ = args.next(); // ignore program name
    let config_path = args.next().unwrap();

    // Read config file
    let mut file = OpenOptions::new().read(true).open(config_path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let config: Config = ron::from_str(&contents)?;
    log::debug!("Loaded config file.");

    // Setup store
    let store = Store::new(&config.data_directory)?;
    let _ = GLOBALS.store.set(store);

    // TLS setup
    let maybe_tls_acceptor = if config.use_tls {
        log::info!("Using TLS");
        Some(tls::tls_acceptor(&config)?)
    } else {
        log::info!("Not using TLS");
        None
    };

    // Bind listener to port
    let listener = TcpListener::bind((&*config.ip_address, config.port)).await?;
    log::info!("Running on {}:{}", config.ip_address, config.port);

    // Store config into GLOBALS
    *GLOBALS.config.write().await = config;

    // Accepts network connections and spawn a task to serve each one
    loop {
        let (tcp_stream, peer_addr) = listener.accept().await?;

        if let Some(tls_acceptor) = &maybe_tls_acceptor {
            let tls_acceptor_clone = tls_acceptor.clone();
            tokio::spawn(async move {
                match tls_acceptor_clone.accept(tcp_stream).await {
                    Err(e) => log::error!("{}", e),
                    Ok(tls_stream) => {
                        if let Err(e) = serve(MaybeTlsStream::Rustls(tls_stream), peer_addr).await {
                            log::error!("{}", e);
                        }
                    }
                }
            });
        } else {
            serve(MaybeTlsStream::Plain(tcp_stream), peer_addr).await?;
        }
    }
}

// Serve a single network connection
async fn serve(stream: MaybeTlsStream<TcpStream>, peer_addr: SocketAddr) -> Result<(), Error> {
    // Serve the network stream with our http server and our HttpService
    let service = HttpService { peer: peer_addr };

    let connection = GLOBALS.http_server.serve_connection(stream, service);

    tokio::spawn(async move {
        // If our service exits with an error, log the error
        if let Err(he) = connection.await {
            if let Some(src) = he.source() {
                if &*format!("{}", src) == "Transport endpoint is not connected (os error 107)" {
                    // do nothing
                } else {
                    // Print in detail
                    log::error!("{:?}", src);
                }
            } else {
                // Print in less detail
                let e: Error = he.into();
                log::error!("{}", e);
            }
        }
    });

    Ok(())
}

// This is our per-connection HTTP service
struct HttpService {
    peer: SocketAddr,
}

impl Service<Request<Body>> for HttpService {
    type Response = Response<Body>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    // This is called for each HTTP request made by the client
    // NOTE: it is not called for each websocket message once upgraded.
    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let peer = self.peer;
        Box::pin(async move { handle_http_request(peer, req).await })
    }
}

async fn handle_http_request(
    _peer: SocketAddr,
    request: Request<Body>,
) -> Result<Response<Body>, Error> {
    // check for Accept header of application/nostr+json
    if let Some(accept) = request.headers().get("Accept") {
        if let Ok(s) = accept.to_str() {
            if s == "application/nostr+json" {
                return web::serve_nip11().await;
            }
        }
    }

    web::serve_http().await
}
