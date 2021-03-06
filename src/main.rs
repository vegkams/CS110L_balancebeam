mod request;
mod response;
mod rate_limiter;

use clap::Parser;
use rand::{Rng, SeedableRng};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::time::{delay_for, Duration};
use std::sync::{Arc, Mutex};
use std::io::{Error, ErrorKind};
use crate::rate_limiter::fixed_window::FixedWindow;
use crate::rate_limiter::{RateLimiterAlgorithm, ArgRateLimiter};

/// Contains information parsed from the command-line invocation of balancebeam. The Clap macros
/// provide a fancy way to automatically construct a command-line argument parser.
#[derive(Parser, Debug)]
#[clap(about = "Fun with load balancing")]
struct CmdOptions {
    #[clap(
        short,
        long,
        help = "IP/port to bind to",
        default_value = "0.0.0.0:1100"
    )]
    bind: String,
    #[clap(short, long, help = "Upstream host to forward requests to")]
    upstream: Vec<String>,
    #[clap(
        long,
        help = "Perform active health checks on this interval (in seconds)",
        default_value = "10"
    )]
    active_health_check_interval: usize,
    #[clap(
        long,
        help = "Path to send request to for active health checks",
        default_value = "/"
    )]
    active_health_check_path: String,
    #[clap(
        long,
        help = "Maximum number of requests to accept per IP per minute (0 = unlimited)",
        default_value = "0"
    )]
    max_requests_per_minute: usize,
    #[clap(
        arg_enum,
        help = "The rate limit algorithm to apply (if max number of requests > 0)",
        default_value = "fixed_window",
    )]
    rate_limiter: ArgRateLimiter,
}

/// Contains information about the state of balancebeam (e.g. what servers we are currently proxying
/// to, what servers have failed, rate limiting counts, etc.)
///
/// You should add fields to this struct in later milestones.
struct ProxyState {
    /// How frequently we check whether upstream servers are alive (Milestone 4)
    #[allow(dead_code)]
    active_health_check_interval: usize,
    /// Where we should send requests when doing active health checks (Milestone 4)
    #[allow(dead_code)]
    active_health_check_path: String,

    upstreams_state: RwLock<UpstreamsState>,
    /// Maximum number of requests an individual IP can make in a minute (Milestone 5)
    #[allow(dead_code)]
    max_requests_per_minute: usize,
    /// Addresses of servers that we are proxying to
    upstream_addresses: Vec<String>,
    /// Rate limiter
    rate_limiter: Mutex<Box<dyn RateLimiterAlgorithm>>
}

struct UpstreamsState {
    num_upstreams: usize,
    status: Vec<bool>,
}

impl UpstreamsState {
    fn new(num_upstreams: usize) -> UpstreamsState {
        UpstreamsState {
            num_upstreams: num_upstreams,
            status: vec![true; num_upstreams], 
        }
    }

    fn is_alive(&self, idx: usize) -> bool {
        self.status[idx]
    }

    fn all_dead(&self) -> bool {
        self.num_upstreams == 0
    }

    fn set_dead(&mut self, idx: usize) {
        if self.is_alive(idx) {
            self.status[idx] = false;
            self.num_upstreams -= 1;
        }
    }

    fn set_alive(&mut self, idx: usize) {
        if !self.is_alive(idx) {
            self.status[idx] = true;
            self.num_upstreams += 1;
        }
    }
}

#[tokio::main]
async fn main() {
    // Initialize the logging library. You can print log messages using the `log` macros:
    // https://docs.rs/log/0.4.8/log/ You are welcome to continue using print! statements; this
    // just looks a little prettier.
    if let Err(_) = std::env::var("RUST_LOG") {
        std::env::set_var("RUST_LOG", "debug");
    }
    pretty_env_logger::init();

    // Parse the command line arguments passed to this program
    let options = CmdOptions::parse();
    if options.upstream.len() < 1 {
        log::error!("At least one upstream server must be specified using the --upstream option.");
        std::process::exit(1);
    }

    // Start listening for connections
    let mut listener = match TcpListener::bind(&options.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            log::error!("Could not bind to {}: {}", options.bind, err);
            std::process::exit(1);
        }
    };
    log::info!("Listening for requests on {}", options.bind);

    let num_upstreams = options.upstream.len();
    // Handle incoming connections
    let state = ProxyState {
        upstream_addresses: options.upstream,
        active_health_check_interval: options.active_health_check_interval,
        active_health_check_path: options.active_health_check_path,
        upstreams_state: RwLock::new(UpstreamsState::new(num_upstreams)),
        max_requests_per_minute: options.max_requests_per_minute,
        rate_limiter: Mutex::new(create_rate_limiter(options.max_requests_per_minute, options.rate_limiter)),
    };

    let shared_state = Arc::new(state);

    let shared_state_health_check = shared_state.clone();
    tokio::spawn(async move {
        active_health_check(shared_state_health_check).await
    });


    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let shared_state_ref = shared_state.clone();
                // Handle the connection!
                tokio::spawn(async move {
                    handle_connection(stream, shared_state_ref).await
                });
            }
            Err(_) => { break; }
        }
    }
}

fn create_rate_limiter(limit: usize, limiter: ArgRateLimiter) -> Box<dyn RateLimiterAlgorithm> {
    match limiter {
        ArgRateLimiter::FixedWindow => {
            Box::new(FixedWindow::new(limit))
        }
    }
}

fn update_rate_limiter(state: Arc<ProxyState>) {
    
}

async fn active_health_check(state: Arc<ProxyState>) {
    let path = &state.active_health_check_path;
    let interval = state.active_health_check_interval as u64;
    loop {
        delay_for(Duration::from_secs(interval)).await;
        let mut upstream_status = state.upstreams_state.write().await;
        for idx in 0..state.upstream_addresses.len() {
            if check_server_status(&state, idx, path).await.is_some() {
                upstream_status.set_alive(idx);
            }
            else {
                upstream_status.set_dead(idx);
            }
        }
    }

}

async fn check_server_status(state: &Arc<ProxyState>, idx: usize, path: &String) -> Option<bool> {
    let ip = &state.upstream_addresses[idx];
    match TcpStream::connect(ip).await {
        Err(e) => None,
        Ok(mut str) => {
            let req = http::Request::builder()
                .method(http::Method::GET)
                .uri(path)
                .header("Host", ip)
                .body(Vec::new())
                .unwrap();
            let _ = request::write_to_stream(&req, &mut str).await.ok()?;
            let res = response::read_from_stream(&mut str, &http::Method::GET).await.ok()?;
            if res.status().as_u16() != 200 {
                None
            } else {
                Some(true)
            }
        },
    }
}


async fn connect_to_upstream(state: Arc<ProxyState>) -> Result<TcpStream, std::io::Error> {
    loop {
        if state.upstreams_state.read().await.all_dead() {
            return Err(std::io::Error::new(ErrorKind::Other, "All upstream servers are dead"));
        }

        let mut rng = rand::rngs::StdRng::from_entropy();
        let upstream_idx: usize = 0;
        loop {
            let upstream_idx = rng.gen_range(0, state.upstream_addresses.len());
            if state.upstreams_state.read().await.is_alive(upstream_idx) {
                break;
            }
        }
        let upstream_ip = &state.upstream_addresses[upstream_idx];

        match TcpStream::connect(upstream_ip).await {
            Err(err) => { log::warn!("Failed to connect to upstream: {:?}", err);
                          let mut upstream_status = state.upstreams_state.write().await;
                          upstream_status.set_dead(upstream_idx);
                        },
            Ok(s) => return Ok(s),
        }
    }
    // TODO: implement failover (milestone 3)
}

async fn send_response(client_conn: &mut TcpStream, response: &http::Response<Vec<u8>>) {
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    log::info!("{} <- {}", client_ip, response::format_response_line(&response));
    if let Err(error) = response::write_to_stream(&response, client_conn).await {
        log::warn!("Failed to send response to client: {}", error);
        return;
    }
}

async fn handle_connection(mut client_conn: TcpStream, state: Arc<ProxyState>) {
    let client_ip = client_conn.peer_addr().unwrap().ip().to_string();
    log::info!("Connection received from {}", client_ip);

    // Open a connection to a random destination server
    let mut upstream_conn = match connect_to_upstream(state).await {
        Ok(stream) => stream,
        Err(_error) => {
            let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
            send_response(&mut client_conn, &response).await;
            return;
        }
    };
    let upstream_ip = client_conn.peer_addr().unwrap().ip().to_string();

    // The client may now send us one or more requests. Keep trying to read requests until the
    // client hangs up or we get an error.
    loop {
        // Read a request from the client
        let mut request = match request::read_from_stream(&mut client_conn).await {
            Ok(request) => request,
            // Handle case where client closed connection and is no longer sending requests
            Err(request::Error::IncompleteRequest(0)) => {
                log::debug!("Client finished sending requests. Shutting down connection");
                return;
            }
            // Handle I/O error in reading from the client
            Err(request::Error::ConnectionError(io_err)) => {
                log::info!("Error reading request from client stream: {}", io_err);
                return;
            }
            Err(error) => {
                log::debug!("Error parsing request: {:?}", error);
                let response = response::make_http_error(match error {
                    request::Error::IncompleteRequest(_)
                    | request::Error::MalformedRequest(_)
                    | request::Error::InvalidContentLength
                    | request::Error::ContentLengthMismatch => http::StatusCode::BAD_REQUEST,
                    request::Error::RequestBodyTooLarge => http::StatusCode::PAYLOAD_TOO_LARGE,
                    request::Error::ConnectionError(_) => http::StatusCode::SERVICE_UNAVAILABLE,
                });
                send_response(&mut client_conn, &response).await;
                continue;
            }
        };
        log::info!(
            "{} -> {}: {}",
            client_ip,
            upstream_ip,
            request::format_request_line(&request)
        );

        // Add X-Forwarded-For header so that the upstream server knows the client's IP address.
        // (We're the ones connecting directly to the upstream server, so without this header, the
        // upstream server will only know our IP, not the client's.)
        request::extend_header_value(&mut request, "x-forwarded-for", &client_ip);

        // Forward the request to the server
        if let Err(error) = request::write_to_stream(&request, &mut upstream_conn).await {
            log::error!("Failed to send request to upstream {}: {}", upstream_ip, error);
            let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
            send_response(&mut client_conn, &response).await;
            return;
        }
        log::debug!("Forwarded request to server");

        // Read the server's response
        let response = match response::read_from_stream(&mut upstream_conn, request.method()).await {
            Ok(response) => response,
            Err(error) => {
                log::error!("Error reading response from server: {:?}", error);
                let response = response::make_http_error(http::StatusCode::BAD_GATEWAY);
                send_response(&mut client_conn, &response).await;
                return;
            }
        };
        // Forward the response to the client
        send_response(&mut client_conn, &response).await;
        log::debug!("Forwarded response to client");
    }
}