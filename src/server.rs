use std::collections::HashMap;

use atty;
use env_logger;
use futures::{Future, Stream};
use futures_cpupool::CpuPool;
use hyper::{Chunk, Client, server::Http};
use leak::Leak;
use tokio_core::reactor::Core;
use tokio::net::TcpListener;
use hyper;

use boot_message;
use config::Config;
use service;

/// Start server with given configuration.
///
/// Server will bind to a port and block.
pub fn serve(config: &Config) {
    env_logger::init();

    let mut core = Core::new().unwrap();
    let handle = core.handle();

    // Leak all of our statics so they're easy to pass down the middleware chain.
    let handle = Box::new(handle).leak();
    let pool = Box::new(CpuPool::new(1)).leak();
    let config = Box::new(config.clone()).leak();
    let client = Box::new(Client::new(handle)).leak();
    let sites = {
        let mut map = HashMap::new();
        for site in config.clone().sites {
            for host in &site.host {
                map.insert(host.clone(), site.clone());
            }
        }
        Box::new(map).leak()
    };

    let mut http: Http<Chunk> = Http::new();
    http.sleep_on_errors(true);

    let listener = TcpListener::bind(&config.server.bind).unwrap();
    let factory = move |remote_ip| service::root::Root {
        client,
        config,
        sites,
        remote_ip,
        pool,
        handle,
    };

    let future = listener.incoming().for_each(move |socket| {
        // TODO: When does socket.peer_addr() fail and how should I handle it?
        let peer = match socket.peer_addr() {
            Err(e) => {
                error!("failed to get peer addr from socket: {}", e);
                return Ok(())
            },
            Ok(peer) => peer,
        };

        let conn = http.serve_connection(socket, factory(peer.ip()))
            .map(|_| ())
            .map_err(|e| {
                use std::io::ErrorKind::BrokenPipe;

                // Silence noisy epipe errors
                match e {
                    hyper::Error::Io(ref e) if e.kind() == BrokenPipe => {},
                    e => error!("server connection error: {}", e)
                }
            });

        handle.spawn(conn);
        Ok(())
    });

    if atty::is(atty::Stream::Stdout) {
        boot_message::pretty(config);
    } else {
        info!("[prox] listening on http://{}", config.server.bind);
    }

    core.run(future).unwrap()
}
