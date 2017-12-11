mod add_host;
mod all_metrics;
mod disk;
mod error_page;
mod processes;
mod query;
mod quick_reply;
mod routing;
mod sockets;
mod status;
mod peers;

use std::sync::{Arc, RwLock};

use futures::Future;
use gossip::Gossip;
use self_meter_http::Meter;
use tk_http::server::{Codec as CodecTrait, Dispatcher as DispatcherTrait};
use tk_http::server::{Error, Head, EncoderDone};
use tk_http::{Status as Http};
use tokio_io::AsyncWrite;

use stats::Stats;
use frontend::routing::{route, Route};
pub use frontend::quick_reply::{reply, read_json};
pub use frontend::error_page::serve_error_page;


pub type Request<S> = Box<CodecTrait<S, ResponseFuture=Reply<S>>>;
pub type Reply<S> = Box<Future<Item=EncoderDone<S>, Error=Error>>;


pub struct Dispatcher {
    pub meter: Meter,
    pub stats: Arc<RwLock<Stats>>,
    pub gossip: Gossip,
}


impl<S: AsyncWrite + Send + 'static> DispatcherTrait<S> for Dispatcher {
    type Codec = Request<S>;
    fn headers_received(&mut self, headers: &Head)
        -> Result<Self::Codec, Error>
    {
        use self::Route::*;
        match route(headers) {
            Index => {
                disk::index_response(headers)
            }
            Static(path) => {
                disk::common_response(headers, path)
            }
            NotFound => {
                serve_error_page(Http::NotFound)
            }
            WebSocket => {
                serve_error_page(Http::NotImplemented)
            }
            Status(format) => {
                Ok(status::serve(&self.meter, &self.stats, format))
            }
            AllProcesses(format) => {
                Ok(processes::serve(&self.stats, format))
            }
            AllSockets(format) => {
                Ok(sockets::serve(&self.stats, format))
            }
            AllMetrics(_) => {
                Ok(all_metrics::serve(&self.stats))
            }
            AllPeers(format) => {
                Ok(peers::serve(&self.gossip, format))
            }
            PeersWithRemote(format) => {
                Ok(peers::serve_only_remote(&self.gossip, format))
            }
            RemoteStats(_) => {
                serve_error_page(Http::NotImplemented)
            }
            StartRemote(_) => {  // POST
                serve_error_page(Http::NotImplemented)
            }
            Query(format) => {   // POST
                Ok(query::serve(&self.stats, format))
            }
            AddHost(format) => { // POST
                Ok(add_host::add_host(&self.gossip, format))
            }
            Remote(_, _) => {
                serve_error_page(Http::NotImplemented)
            }
        }
    }
}
