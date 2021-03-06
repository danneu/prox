use std::io;
use std::net::IpAddr;

use futures::{Future, future::ok};
use futures::future::Either;
use hyper::{self, header, Client, Request, Response, Uri, client::HttpConnector, server::Service};
use tokio_core::reactor::Handle;
use tokio_core::reactor::Timeout;

use config::{Config, Site};
use hop;
use response;

header! {
    (XForwardedFor, "X-Forwarded-For") => (IpAddr)+
}

pub struct Proxy {
    pub client: &'static Client<HttpConnector>,
    pub remote_ip: IpAddr,
    pub config: &'static Config,
    pub handle: &'static Handle,
}

/// Return a new headers map with any hop-to-hop headers removed.
fn without_hop_headers(headers: &header::Headers) -> header::Headers {
    headers
        .iter()
        .filter(|h| !hop::is_hop_header(h.name()))
        .collect()
}

fn make_proxy_request(mut req: Request, uri: Uri, remote_ip: IpAddr) -> Request {
    req.set_uri(uri);

    *req.headers_mut() = without_hop_headers(req.headers());

    // Update forwarded-for header
    match req.headers_mut().get_mut::<XForwardedFor>() {
        Some(ips) => ips.push(remote_ip),
        None => req.headers_mut().set(XForwardedFor(vec![remote_ip])),
    }

    req
}

fn make_proxy_response(mut res: Response) -> Response {
    *res.headers_mut() = without_hop_headers(res.headers());
    res
}

impl Service for Proxy {
    type Request = (&'static Site, Request);
    type Response = Response;
    type Error = hyper::Error;
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn call(&self, (site, req): Self::Request) -> Self::Future {
        // Proxy only enabled if site.url is given.
        let site_url = match site.url {
            None => return Box::new(ok(response::not_found())),
            Some(ref url) => url,
        };

        // Concatenate site url and request path into target uri
        let uri = site_url
            .join(req.path())
            .ok()
            .and_then(|url| url.to_string().parse::<Uri>().ok());

        // Bail if it doesn't parse into a uri
        let uri = match uri {
            Some(x) => x,
            None => return Box::new(ok(response::not_found())),
        };

        let proxy_req = make_proxy_request(req, uri, self.remote_ip);
        trace!("proxy_req: {:#?}", proxy_req);

        // Set up timeouts and make the proxied request

        let conn_duration = self.config.server.timeouts.connect;

        let conn_timeout = match Timeout::new(conn_duration, self.handle) {
            Ok(x) => x,
            Err(e) => {
                error!("error creating timeout: {}", e);
                return Box::new(ok(response::internal_server_error()))
            },
        };

        // The future of the origin's response
        let res_future = self.client.request(proxy_req).then(|res| match res {
            Ok(res) => Ok(make_proxy_response(res)),
            Err(e) => {
                error!("error making client request: {:?}", e);
                match e {
                    // TODO: How should other errors be handled?
                    hyper::Error::Io(ref e) if e.kind() == io::ErrorKind::ConnectionRefused
                        || e.kind() == io::ErrorKind::ConnectionAborted
                        || e.kind() == io::ErrorKind::ConnectionReset =>
                        Ok(response::bad_gateway()),
                    _ =>
                        Ok(response::internal_server_error()),
                }
            },
        });

        let future = res_future
            .select2(conn_timeout)
            .then(|result| match result {
                Ok(Either::A((res, _err))) => Ok(res),
                Ok(Either::B((_timeout_error, _res))) => {
                    // TODO: Look into future lifecycle. Surely I don't need to drop(res_future) myself?
                    Err(hyper::Error::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "[timeout] client timed out during connect",
                    )))
                }
                Err(Either::A((res_error, _))) => Err(res_error),
                Err(Either::B((timeout_error, _res))) => Err(From::from(timeout_error)),
            });

        Box::new(future)
    }
}
